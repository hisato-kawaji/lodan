use anyhow::Result;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::hooks::{self, HookOutcome, Lifecycle};
use crate::llm;
use crate::mcp;
use crate::mcp::prompt::McpPrompt;
use crate::permission::PermissionGate;
use crate::session::Recorder;
use crate::slash::{self, SlashCommand};
use crate::tools::registry::default_registry;

/// REPL 組み込みコマンド。ユーザ定義コマンドより優先する。
const BUILTINS: &[&str] = &[
    "exit", "quit", "help", "clear", "tools", "compact", "cost", "goal", "loop",
];

/// `/goal` の解除サブコマンド別名 (Claude Code と同じ)。
const GOAL_CLEAR_ALIASES: &[&str] = &["clear", "stop", "off", "reset", "none", "cancel"];

pub async fn run(cfg: Config, resume: Option<String>) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    println!(
        "{} {} — type {} for commands, {} to quit",
        crate::term::bold(&crate::term::cyan("lodan")),
        env!("CARGO_PKG_VERSION"),
        crate::term::cyan("/help"),
        crate::term::cyan("/exit"),
    );
    let active = cfg.llm.active();
    println!(
        "{}",
        crate::term::dim(&format!(
            "model: {} @ {} ({})",
            active.model,
            active.base_url,
            cfg.llm.provider.as_str()
        ))
    );

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let user_commands = load_user_commands(&cwd.join(".lodan/commands"));
    if !user_commands.is_empty() {
        println!("slash: {} user command(s) loaded", user_commands.len());
    }

    let user_skills = crate::skills::load_from(&cwd.join(".lodan/skills")).unwrap_or_else(|e| {
        eprintln!("skills: load failed: {e}");
        Vec::new()
    });
    if !user_skills.is_empty() {
        println!("skills: {} loaded", user_skills.len());
    }

    let llm_client: Arc<dyn llm::LlmClient> = llm::build_client(&cfg)?;

    let mut registry = default_registry();
    // sampling は opt-in サーバにのみ active モデルの LLM を貸す。
    let sampling_ctx = mcp::registry::SamplingContext {
        llm: Arc::clone(&llm_client),
        model: cfg.llm.active().model.clone(),
    };
    let mcp_outcome = mcp::registry::load_and_register(&mut registry, Some(sampling_ctx))
        .await
        .unwrap_or_else(|e| {
            eprintln!("mcp: {e}");
            mcp::registry::LoadOutcome::default()
        });
    if mcp_outcome.servers > 0 {
        println!(
            "mcp: {} server(s), {} tool(s), {} prompt(s), {} resource(s) registered",
            mcp_outcome.servers,
            mcp_outcome.tools,
            mcp_outcome.prompts.len(),
            mcp_outcome.resources
        );
    }
    let mcp_prompts: BTreeMap<String, McpPrompt> = mcp_outcome
        .prompts
        .into_iter()
        .map(|p| (p.full_name().to_string(), p))
        .collect();
    // Keep clients alive for the full session; Drop kills subprocesses.
    let _mcp_clients = mcp_outcome.clients;

    // サブエージェント (Task): 読み取り専用ツールで調査を委譲する。
    // LLM クライアントが要るため default_registry ではなくここで登録する。
    let sub_tools = Arc::new(crate::tools::registry::read_only_registry());
    registry.register(Arc::new(agent::subagent::SubAgentTool::new(
        Arc::clone(&llm_client),
        cfg.llm.active().model.clone(),
        sub_tools,
        cwd.clone(),
        cfg.agent.max_iterations,
    )));

    // Skill ツール: モデルが名前で手順書を読み込める。skill が無ければ登録しない。
    if !user_skills.is_empty() {
        registry.register(Arc::new(crate::skills::SkillTool::new(user_skills)));
    }

    let registry = Arc::new(registry);
    let gate = PermissionGate::new(cfg.agent.auto_approve);

    let (mut session, mut recorder) = match resume {
        Some(arg) => resume_session(&arg, &cfg, &registry),
        None => new_session(&cwd, &cfg, &registry),
    };

    // /goal の状態。上限到達などで未達のまま止まった goal は paused として残り、
    // `/goal` (状態表示) と `/goal clear` (解除) の対象になる。
    let mut goal_state: Option<crate::goal::Goal> = None;

    // SessionStart hook: 起動を通知する。ブロックされても起動は止めず警告のみ。
    let session_payload =
        serde_json::json!({ "hook_event_name": "SessionStart", "cwd": cwd.display().to_string() });
    if let Ok(HookOutcome::Block(reason)) =
        hooks::runner::dispatch(Lifecycle::SessionStart, None, &session_payload, &cfg.hooks).await
    {
        eprintln!("session-start hook: {reason}");
    }

    loop {
        let line = match rl.readline("lodan> ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => {
                println!("(Ctrl-C, type /exit to quit)");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);

        if let Some(rest) = line
            .strip_prefix('/')
            .filter(|r| looks_like_slash_command(r))
        {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let head = parts.next().unwrap_or("");
            let args = parts.next().unwrap_or("").trim();

            // /compact は session/llm を要するため handle_slash ではなくここで処理する。
            if head == "compact" {
                match session.compact(llm_client.as_ref(), args).await {
                    Ok(outcome) => println!("{}", crate::term::dim(&outcome.describe())),
                    Err(e) => {
                        eprintln!(
                            "{}",
                            crate::term::red_err(&format!("compact failed: {e:#}"))
                        )
                    }
                }
                persist(&mut recorder, &session);
                continue;
            }

            // /cost も session を要するためここで処理する。
            if head == "cost" {
                println!("{}", session.usage().describe());
                continue;
            }

            // /loop も session/llm を要するためここで処理する。
            if head == "loop" {
                handle_loop(
                    args,
                    &user_commands,
                    &mut session,
                    llm_client.as_ref(),
                    &gate,
                    &mut recorder,
                )
                .await;
                continue;
            }

            // /goal も session/llm を要するためここで処理する。
            if head == "goal" {
                handle_goal(
                    args,
                    &mut goal_state,
                    &mut session,
                    llm_client.as_ref(),
                    &cfg.llm.active().model,
                    &gate,
                    &mut recorder,
                )
                .await;
                continue;
            }

            match handle_slash(head, &registry, &user_commands, &mcp_prompts) {
                SlashResult::Exit => break,
                SlashResult::Handled => continue,
                SlashResult::Unknown => {
                    // 組み込みに無ければユーザ定義コマンド → MCP prompt の順に試す。
                    if let Some(cmd) = user_commands.get(head) {
                        let prompt = slash::expand(&cmd.body, args);
                        run_turn_interruptible(&mut session, &prompt, llm_client.as_ref(), &gate)
                            .await;
                        persist(&mut recorder, &session);
                    } else if let Some(mcp_prompt) = mcp_prompts.get(head) {
                        let positional: Vec<&str> = args.split_whitespace().collect();
                        match mcp_prompt.render(&positional).await {
                            Ok(text) if !text.trim().is_empty() => {
                                run_turn_interruptible(
                                    &mut session,
                                    &text,
                                    llm_client.as_ref(),
                                    &gate,
                                )
                                .await;
                                persist(&mut recorder, &session);
                            }
                            Ok(_) => eprintln!("mcp prompt /{head} returned no text"),
                            Err(e) => eprintln!("mcp prompt /{head} failed: {e:#}"),
                        }
                    } else {
                        eprintln!("unknown command: /{head}");
                    }
                    continue;
                }
            }
        }

        run_turn_interruptible(&mut session, line, llm_client.as_ref(), &gate).await;
        persist(&mut recorder, &session);
    }

    // SessionEnd hook: 終了を通知する（ブロック不能・ベストエフォート）。
    let end_payload = serde_json::json!({ "hook_event_name": "SessionEnd" });
    let _ = hooks::runner::dispatch(Lifecycle::SessionEnd, None, &end_payload, &cfg.hooks).await;

    Ok(())
}

/// 新規セッションを作り、永続化レコーダを用意する。
/// レコーダ作成に失敗してもセッションは続行する (永続化なしの ephemeral)。
fn new_session(
    cwd: &std::path::Path,
    cfg: &Config,
    registry: &Arc<crate::tools::registry::ToolRegistry>,
) -> (agent::Session, Option<Recorder>) {
    let session = agent::Session::new(cfg.clone(), Arc::clone(registry));
    let recorder = match Recorder::create(cwd, cfg.llm.provider.as_str(), &cfg.llm.active().model) {
        Ok(r) => {
            println!("session: {}", r.id());
            Some(r)
        }
        Err(e) => {
            eprintln!("session: persistence disabled ({e})");
            None
        }
    };
    (session, recorder)
}

/// 保存済みセッションを復元する。失敗時は警告して新規セッションにフォールバックする。
fn resume_session(
    arg: &str,
    cfg: &Config,
    registry: &Arc<crate::tools::registry::ToolRegistry>,
) -> (agent::Session, Option<Recorder>) {
    let resolved = if arg == "last" {
        crate::session::latest_session_id().ok().flatten()
    } else {
        Some(arg.to_string())
    };

    let Some(id) = resolved else {
        eprintln!("session: no session to resume");
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return new_session(&cwd, cfg, registry);
    };

    match crate::session::load_transcript(&id) {
        Ok(prior) => {
            let n = prior.len();
            let session = agent::Session::resume(cfg.clone(), Arc::clone(registry), prior);
            // recorder は復元後の history を基準に「保存済み」位置を決める。
            match Recorder::open_resumed(&id, session.history()) {
                Ok(recorder) => {
                    println!("session: resumed {id} ({n} messages)");
                    (session, Some(recorder))
                }
                Err(e) => {
                    eprintln!("session: resumed {id} but persistence disabled ({e})");
                    (session, None)
                }
            }
        }
        Err(e) => {
            eprintln!("session: cannot resume {id}: {e}");
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            new_session(&cwd, cfg, registry)
        }
    }
}

/// `/goal` builtin。引数なし = 状態表示、解除別名 = 解除、それ以外 = 条件設定＋
/// 達成までの自律ループ開始。破壊的ツールは既存の承認ゲートをそのまま通る
/// (自動承認したいときは `--yes` / `auto_approve`)。
async fn handle_goal(
    args: &str,
    goal_state: &mut Option<crate::goal::Goal>,
    session: &mut agent::Session,
    llm: &dyn llm::LlmClient,
    model: &str,
    gate: &PermissionGate,
    recorder: &mut Option<Recorder>,
) {
    use crate::goal::{Goal, GoalOutcome};

    // 状態表示
    if args.is_empty() {
        match goal_state {
            Some(g) => println!("goal (paused):\n{}", g.describe()),
            None => println!("no active goal — set one with /goal <condition>"),
        }
        return;
    }

    // 解除
    if GOAL_CLEAR_ALIASES.contains(&args) {
        match goal_state.take() {
            Some(g) => println!("goal cleared: {}", first_line(&g.condition)),
            None => println!("no active goal to clear"),
        }
        return;
    }

    // 設定＋実行
    let mut goal = match Goal::new(args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{}", crate::term::red_err(&format!("goal: {e:#}")));
            return;
        }
    };
    if let Some(old) = goal_state.take() {
        println!("goal replaced: {}", first_line(&old.condition));
    }
    println!(
        "{}",
        crate::term::dim(&format!(
            "[goal] started (limits: {} turns / {}s). destructive tools still ask for approval unless --yes",
            goal.max_turns,
            goal.max_duration.as_secs()
        ))
    );

    // Ctrl-C で自律ループごと中断できるようにする (ターン途中なら履歴を修復)。
    let outcome = {
        let fut = crate::goal::drive(&mut goal, session, llm, model, gate, |s| {
            if let Some(rec) = recorder.as_mut()
                && let Err(e) = rec.sync(s.history())
            {
                eprintln!("session: save failed: {e}");
            }
        });
        tokio::pin!(fut);
        tokio::select! {
            out = &mut fut => Some(out),
            _ = wait_ctrl_c() => None,
        }
    };
    let Some(outcome) = outcome else {
        session.interrupt_repair();
        if let Some(rec) = recorder.as_mut()
            && let Err(e) = rec.sync(session.history())
        {
            eprintln!("session: save failed: {e}");
        }
        println!();
        println!(
            "{}",
            crate::term::red(
                "[goal] interrupted by Ctrl-C — paused (/goal to inspect, /goal clear to drop)"
            )
        );
        *goal_state = Some(goal);
        return;
    };

    match outcome {
        GoalOutcome::Achieved { reason, turns } => {
            println!(
                "{}",
                crate::term::bold(&crate::term::cyan(&format!(
                    "[goal] achieved after {turns} turn(s): {reason}"
                )))
            );
            // 達成した goal は解除する。
        }
        GoalOutcome::TurnLimit => {
            println!(
                "{}",
                crate::term::red(&format!(
                    "[goal] stopped: turn limit ({}) reached — /goal to inspect, /goal clear to drop",
                    goal.max_turns
                ))
            );
            *goal_state = Some(goal);
        }
        GoalOutcome::TimeLimit => {
            println!(
                "{}",
                crate::term::red(&format!(
                    "[goal] stopped: time limit ({}s) reached — /goal to inspect, /goal clear to drop",
                    goal.max_duration.as_secs()
                ))
            );
            *goal_state = Some(goal);
        }
        GoalOutcome::EvaluatorFailed(e) => {
            eprintln!(
                "{}",
                crate::term::red_err(&format!("[goal] stopped: evaluator failed: {e:#}"))
            );
            *goal_state = Some(goal);
        }
        GoalOutcome::TurnFailed(e) => {
            eprintln!(
                "{}",
                crate::term::red_err(&format!("[goal] stopped: turn failed: {e:#}"))
            );
            *goal_state = Some(goal);
        }
    }
}

/// `/loop` builtin。書式: `/loop <interval> <prompt|/usercmd [args]>`。
/// フォアグラウンドで固定間隔の反復を回す (REPL を占有、Ctrl-C で停止)。
/// 破壊的ツールは /goal と同じく既存の承認ゲートをそのまま通る。
async fn handle_loop(
    args: &str,
    user_commands: &BTreeMap<String, SlashCommand>,
    session: &mut agent::Session,
    llm: &dyn llm::LlmClient,
    gate: &PermissionGate,
    recorder: &mut Option<Recorder>,
) {
    use crate::loop_cmd::{LoopOutcome, LoopSpec, drive, parse_interval};

    let mut parts = args.splitn(2, char::is_whitespace);
    let interval_tok = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    let interval = match parse_interval(interval_tok) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "{}",
                crate::term::red_err(&format!(
                    "loop: {e:#}\nusage: /loop <interval> <prompt|/usercmd [args]> (e.g. /loop 5m run the tests)"
                ))
            );
            return;
        }
    };

    // 先頭が slash ならユーザ定義コマンドとして 1 度だけ展開し、以後は毎反復同じ
    // プロンプトを再投入する (組み込み slash の反復は対象外)。
    let prompt = if let Some(cmd_rest) = rest.strip_prefix('/') {
        let mut cp = cmd_rest.splitn(2, char::is_whitespace);
        let name = cp.next().unwrap_or("");
        let cmd_args = cp.next().unwrap_or("").trim();
        match user_commands.get(name) {
            Some(cmd) => slash::expand(&cmd.body, cmd_args),
            None => {
                eprintln!(
                    "{}",
                    crate::term::red_err(&format!(
                        "loop: unknown command /{name} (only user-defined commands can be looped)"
                    ))
                );
                return;
            }
        }
    } else {
        rest.to_string()
    };

    let spec = match LoopSpec::new(interval, &prompt) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", crate::term::red_err(&format!("loop: {e:#}")));
            return;
        }
    };
    println!(
        "{}",
        crate::term::dim(&format!(
            "[loop] started (limits: {} iterations / {}h). Ctrl-C to stop; destructive tools still ask for approval unless --yes",
            spec.max_iterations,
            spec.max_duration.as_secs() / 3600
        ))
    );

    // Ctrl-C でループごと中断できるようにする (ターン途中なら履歴を修復)。
    let outcome = {
        let fut = drive(&spec, session, llm, gate, |s| {
            if let Some(rec) = recorder.as_mut()
                && let Err(e) = rec.sync(s.history())
            {
                eprintln!("session: save failed: {e}");
            }
        });
        tokio::pin!(fut);
        tokio::select! {
            out = &mut fut => Some(out),
            _ = wait_ctrl_c() => None,
        }
    };
    match outcome {
        None => {
            session.interrupt_repair();
            persist(recorder, session);
            println!();
            println!("{}", crate::term::red("[loop] interrupted by Ctrl-C"));
        }
        Some(LoopOutcome::IterationLimit { iterations }) => println!(
            "{}",
            crate::term::red(&format!(
                "[loop] stopped: iteration limit reached ({iterations})"
            ))
        ),
        Some(LoopOutcome::TimeLimit { iterations }) => println!(
            "{}",
            crate::term::red(&format!(
                "[loop] stopped: time limit reached after {iterations} iteration(s)"
            ))
        ),
        Some(LoopOutcome::TurnFailed { iterations, error }) => eprintln!(
            "{}",
            crate::term::red_err(&format!(
                "[loop] stopped: turn failed after {iterations} completed iteration(s): {error:#}"
            ))
        ),
    }
}

/// Ctrl-C (SIGINT) を待つ。ハンドラ登録に失敗したときは永久に pending にして
/// select! の相手側 (実行中のターン) を邪魔しない。
async fn wait_ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// run_turn を Ctrl-C で中断可能にして実行する。中断時は future を破棄して
/// in-flight のストリーム/ツールをキャンセルし (foreground Bash の子プロセスは
/// `kill_on_drop` で終了する)、履歴の整合性を修復する。エラーはここで表示する。
async fn run_turn_interruptible(
    session: &mut agent::Session,
    input: &str,
    llm: &dyn llm::LlmClient,
    gate: &PermissionGate,
) {
    let interrupted = {
        let turn = session.run_turn(input, llm, gate);
        tokio::pin!(turn);
        tokio::select! {
            res = &mut turn => {
                if let Err(e) = res {
                    eprintln!("{}", crate::term::red_err(&format!("error: {e:#}")));
                }
                false
            }
            _ = wait_ctrl_c() => true,
        }
    };
    if interrupted {
        session.interrupt_repair();
        println!();
        println!("{}", crate::term::red("(turn interrupted by Ctrl-C)"));
    }
}

/// ターン後に履歴を transcript へ追記する (レコーダ無効時は no-op)。
fn persist(recorder: &mut Option<Recorder>, session: &agent::Session) {
    if let Some(rec) = recorder.as_mut()
        && let Err(e) = rec.sync(session.history())
    {
        eprintln!("session: save failed: {e}");
    }
}

enum SlashResult {
    Exit,
    Handled,
    Unknown,
}

/// Decide whether `rest` (the input after the leading `/`) should be dispatched
/// as a slash command. Anything containing a path separator or whitespace inside
/// the head token is treated as normal LLM input, so prompts that begin with an
/// absolute path (e.g. `/tmp/foo に hi と書いて`) reach the model unchanged.
fn looks_like_slash_command(rest: &str) -> bool {
    let head = rest.split_whitespace().next().unwrap_or("");
    !head.is_empty()
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// ツール説明の 1 行目を最大 90 文字で返す（`/tools` の一覧表示用）。
fn first_line(desc: &str) -> String {
    let line = desc.lines().next().unwrap_or("").trim();
    if line.chars().count() > 90 {
        let head: String = line.chars().take(89).collect();
        format!("{head}…")
    } else {
        line.to_string()
    }
}

fn handle_slash(
    cmd: &str,
    registry: &crate::tools::registry::ToolRegistry,
    user_commands: &BTreeMap<String, SlashCommand>,
    mcp_prompts: &BTreeMap<String, McpPrompt>,
) -> SlashResult {
    match cmd {
        "exit" | "quit" => SlashResult::Exit,
        "help" => {
            println!("{}", crate::term::bold("built-in:"));
            for (name, desc) in [
                ("/exit, /quit", "REPL を終了"),
                ("/help", "このヘルプを表示"),
                ("/clear", "画面をクリア"),
                ("/tools", "利用可能なツール一覧"),
                ("/compact [指示]", "会話履歴を要約して圧縮"),
                ("/cost", "セッション累積のトークン使用量を表示"),
                (
                    "/goal <条件> | /goal | /goal clear",
                    "条件達成までターンを自律継続 / 状態表示 / 解除",
                ),
                (
                    "/loop <間隔> <プロンプト|/cmd>",
                    "固定間隔で反復実行 (5s/5m/2h/1d、Ctrl-C で停止)",
                ),
            ] {
                println!("  {} — {desc}", crate::term::cyan(name));
            }
            if !user_commands.is_empty() {
                println!("{}", crate::term::bold("user commands:"));
                for c in user_commands.values() {
                    let name = crate::term::cyan(&format!("/{}", c.name));
                    if c.description.is_empty() {
                        println!("  {name}");
                    } else {
                        println!("  {name} — {}", c.description);
                    }
                }
            }
            if !mcp_prompts.is_empty() {
                println!("{}", crate::term::bold("mcp prompts:"));
                for p in mcp_prompts.values() {
                    let name = crate::term::cyan(&format!("/{}", p.full_name()));
                    if p.description().is_empty() {
                        println!("  {name}");
                    } else {
                        println!("  {name} — {}", p.description());
                    }
                }
            }
            SlashResult::Handled
        }
        "clear" => {
            print!("\x1b[2J\x1b[H");
            SlashResult::Handled
        }
        "tools" => {
            for spec in registry.tool_specs() {
                let desc = first_line(spec.function.description);
                println!("{} — {desc}", crate::term::cyan(spec.function.name));
            }
            SlashResult::Handled
        }
        _ => SlashResult::Unknown,
    }
}

/// `.lodan/commands/` を読み、組み込みと衝突する名前は警告して除外する。
fn load_user_commands(dir: &std::path::Path) -> BTreeMap<String, SlashCommand> {
    let cmds = match slash::load_dir(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("slash: load failed: {e}");
            return BTreeMap::new();
        }
    };
    let mut map = BTreeMap::new();
    for cmd in cmds {
        if BUILTINS.contains(&cmd.name.as_str()) {
            eprintln!("slash: /{} shadows a builtin, skipped", cmd.name);
            continue;
        }
        map.insert(cmd.name.clone(), cmd);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::looks_like_slash_command;

    #[test]
    fn known_commands_match() {
        for c in ["exit", "quit", "help", "clear", "tools"] {
            assert!(looks_like_slash_command(c), "{c} should be a command");
        }
    }

    #[test]
    fn absolute_paths_are_not_commands() {
        assert!(!looks_like_slash_command("tmp/foo"));
        assert!(!looks_like_slash_command(
            "tmp/lodan-demo/hello.txt に hi と書いて"
        ));
        assert!(!looks_like_slash_command("Users/me/file.rs"));
    }

    #[test]
    fn empty_or_whitespace_is_not_a_command() {
        assert!(!looks_like_slash_command(""));
        assert!(!looks_like_slash_command("   "));
    }

    #[test]
    fn command_with_trailing_args_still_matches() {
        assert!(looks_like_slash_command("help"));
        assert!(looks_like_slash_command("tools "));
        assert!(looks_like_slash_command("tools list"));
    }
}
