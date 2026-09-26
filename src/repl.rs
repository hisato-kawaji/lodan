use anyhow::Result;
use rustyline::Editor;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::hooks::Lifecycle;
use crate::llm;
use crate::mcp::prompt::McpPrompt;
use crate::permission::PermissionGate;
use crate::runtime::{Notices, Runtime};
use crate::session::Recorder;
use crate::slash::{self, SlashCommand};

/// REPL 組み込みコマンド。ユーザ定義コマンドより優先する。
const BUILTINS: &[&str] = &[
    "exit", "quit", "help", "clear", "tools", "compact", "cost", "goal", "loop", "plan", "accept",
    "undo", "memory", "model", "status", "context",
];

/// `/goal` の解除サブコマンド別名 (Claude Code と同じ)。
const GOAL_CLEAR_ALIASES: &[&str] = &["clear", "stop", "off", "reset", "none", "cancel"];
/// `/goal resume`: paused の goal を続きから走らせる。
const GOAL_RESUME_ALIASES: &[&str] = &["resume", "continue"];

/// rustyline の補完ヘルパ (#42 P7)。行頭の `/…` は slash コマンド名を、
/// それ以外の語は FilenameCompleter でパスを補完する。
struct ReplHelper {
    /// 補完対象のコマンド名 (組み込み + ユーザ定義 + MCP prompt)。
    commands: Vec<String>,
    files: FilenameCompleter,
}

impl ReplHelper {
    fn new(mut commands: Vec<String>) -> Self {
        commands.sort();
        Self {
            commands,
            files: FilenameCompleter::new(),
        }
    }
}

/// 行頭 slash コマンドの補完候補。カーソルが最初のトークン内
/// (`/` 直後〜空白前) にあるときだけ Some を返す。前方一致なしでも
/// Some(空) を返す — コマンド位置でパス補完へフォールバックすると
/// `/zzz` が `/usr` 等に化けて紛らわしいため意図的に補完なしとする。
fn slash_candidates(line: &str, pos: usize, commands: &[String]) -> Option<Vec<String>> {
    let head = line.get(..pos)?;
    let rest = head.strip_prefix('/')?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(
        commands
            .iter()
            .filter(|c| c.starts_with(rest))
            .map(|c| format!("/{c}"))
            .collect(),
    )
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if let Some(cands) = slash_candidates(line, pos, &self.commands) {
            let pairs = cands
                .into_iter()
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c,
                })
                .collect();
            // 行頭 `/` ごと置換する。
            return Ok((0, pairs));
        }
        self.files.complete(line, pos, ctx)
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Highlighter for ReplHelper {}

/// 複数行入力 (#42 P8)。Enter 時に入力が「未完」なら改行を挿入して
/// 編集を継続する: ``` フェンスが閉じていない、または行末が `\`。
impl Validator for ReplHelper {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        use rustyline::validate::ValidationResult;
        Ok(if input_needs_more(ctx.input()) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl rustyline::Helper for ReplHelper {}

/// 入力が未完 (継続入力が必要) か。
/// - 先頭行が ``` で始まる: 2 行目以降に閉じ ``` 行が現れるまで未完
/// - それ以外: 末尾が `\` なら未完 (継続行)
fn input_needs_more(input: &str) -> bool {
    let first = input.lines().next().unwrap_or("");
    if first.trim_start().starts_with("```") {
        return !input.lines().skip(1).any(|l| l.trim() == "```");
    }
    input.ends_with('\\')
}

/// 確定した複数行入力を正規化する。
/// - フェンス入力: 先頭の ```(言語タグ可) 行と最後の閉じ ``` 行を外して中身のみ
/// - 継続行入力: 各行末の `\` を除いて改行で連結
/// - 単一行入力はそのまま
fn normalize_input(input: &str) -> String {
    // 単一行はそのまま。対話入力では行末 `\` は Validator が継続させるので
    // 単一行のまま確定しないが、パイプ入力 (非対話) では Validator を通らず
    // ここへ来るため、末尾 `\` を黙って食わないようにする (pr-review #59)。
    if !input.contains('\n') {
        return input.to_string();
    }
    let first = input.lines().next().unwrap_or("");
    if first.trim_start().starts_with("```") {
        let mut lines: Vec<&str> = input.lines().skip(1).collect();
        if let Some(pos) = lines.iter().rposition(|l| l.trim() == "```") {
            lines.remove(pos);
        }
        return lines.join("\n");
    }
    input
        .lines()
        .map(|l| l.strip_suffix('\\').unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run(mut cfg: Config, resume: Option<String>) -> Result<()> {
    let mut rl: Editor<ReplHelper, DefaultHistory> = Editor::new()?;
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
    // プロジェクトの slash コマンドも、信頼済みのディレクトリでだけ読む (#75)。
    let user_commands = if crate::trust::project_trusted() {
        load_user_commands(&cwd.join(".lodan/commands"))
    } else {
        BTreeMap::new()
    };
    if !user_commands.is_empty() {
        println!("slash: {} user command(s) loaded", user_commands.len());
    }

    let runtime = Runtime::build(&cfg, Notices::Stdout).await?;
    // `/model` で作り直す。サブエージェント (Task) と MCP sampling は起動時のクライアントのまま。
    let mut llm_client = Arc::clone(&runtime.llm);
    let registry = Arc::clone(&runtime.registry);
    let mcp_prompts = &runtime.mcp_prompts;

    // 補完対象が出揃ったところで helper を装着する (#42 P7)。
    let completion_names: Vec<String> = BUILTINS
        .iter()
        .map(|s| s.to_string())
        .chain(user_commands.keys().cloned())
        .chain(mcp_prompts.keys().cloned())
        .collect();
    rl.set_helper(Some(ReplHelper::new(completion_names)));

    let gate = PermissionGate::from_config(&cfg, &runtime.cwd, true)?;

    let (mut session, mut recorder) =
        runtime.open_session(&cfg, resume.as_deref(), Notices::Stdout);
    if cfg.permissions.mode == crate::config::PermissionMode::Plan {
        session.set_mode(agent::Mode::Plan);
    }

    // /goal の状態。上限到達などで未達のまま止まった goal は paused として残り、
    // `/goal` (状態表示) と `/goal clear` (解除) の対象になる。
    let mut goal_state: Option<crate::goal::Goal> = None;
    // 再開したセッションに goal が残っていれば、paused として戻す (勝手には走らせない)。
    if let Some(rec) = recorder.as_ref() {
        match rec
            .load_goal()
            .and_then(|r| r.map(crate::goal::Goal::from_record).transpose())
        {
            Ok(Some(goal)) => {
                println!(
                    "{}",
                    crate::term::dim(&format!(
                        "[goal] restored (paused after {} turn(s)): {} — /goal resume to continue, /goal clear to drop",
                        goal.total_turns,
                        crate::term::sanitize(&first_line(&goal.condition))
                    ))
                );
                goal_state = Some(goal);
            }
            Ok(None) => {}
            Err(e) => eprintln!("session: ignoring the saved goal: {e:#}"),
        }
    }

    // SessionStart hook: 起動を通知する。ブロックされても起動は止めず警告のみ。
    let session_source = if resume.is_some() {
        "resume"
    } else {
        "startup"
    };
    match session
        .fire_hook(
            Lifecycle::SessionStart,
            Some(session_source),
            serde_json::json!({ "source": session_source }),
        )
        .await
    {
        Ok(started) => {
            if let Some(reason) = &started.block {
                eprintln!("session-start hook: {}", crate::term::sanitize(reason));
            }
            // plain な stdout と `additionalContext` は、最初のユーザ入力と一緒にモデルへ渡す。
            started
                .context
                .into_iter()
                .for_each(|c| session.add_context(c));
        }
        Err(e) => eprintln!(
            "session-start hook: {}",
            crate::term::sanitize(&e.to_string())
        ),
    }

    loop {
        let prompt = prompt_line(&cfg, &session);
        let line = match rl.readline(&prompt) {
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
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(raw);
        // 複数行入力 (#42 P8): フェンス外し・継続行の連結を済ませてから処理する。
        let line = normalize_input(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // `!<cmd>`: シェルで直接実行して、出力を次のユーザ発話の文脈に添える (#81)。承認ゲートは
        // 通さない — 打ったのは利用者自身で、モデルではない。
        if let Some(cmd) = line.strip_prefix('!').map(str::trim)
            && !cmd.is_empty()
        {
            let output = run_shell_for_user(cmd, &runtime.cwd, cfg.tools.bash.timeout_secs).await;
            println!("{}", crate::term::sanitize(&output));
            session.add_context(format!(
                "The user ran this shell command themselves (not via a tool); its output is context \
                 for their next message:\n$ {cmd}\n{output}"
            ));
            continue;
        }

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
                // 合計と内訳はプロセス全体の台帳から (サブエージェントや /goal の評価器も入る)。
                // 現在のコンテキストの大きさだけは、このセッションのループが知っている。
                println!("{}", runtime.ledger.describe());
                if session.usage().llm_calls > 0 {
                    println!(
                        "last context: {} prompt tokens",
                        session.usage().last_context_tokens
                    );
                }
                continue;
            }

            // /memory: いま読まれているメモリの一覧 (#79)。
            if head == "memory" {
                let loaded = crate::memory::load_memory_detailed(&runtime.cwd);
                if loaded.sources.is_empty() {
                    println!(
                        "no memory files (LODAN.md / CLAUDE.md / AGENTS.md in the cwd hierarchy, ~/.lodan/LODAN.md)"
                    );
                }
                // パスは clone したリポジトリの中の名前かもしれない。端末に出す前に無害化する。
                for s in &loaded.sources {
                    let via = s.imported_from.as_ref().map_or(String::new(), |f| {
                        format!("  (imported from {})", crate::trust::shown(f))
                    });
                    println!("{} — {} bytes{via}", crate::trust::shown(&s.path), s.bytes);
                }
                if !loaded.sources.is_empty() {
                    println!(
                        "total: {} bytes in the system prompt (cap {} bytes){}",
                        loaded.text.len(),
                        crate::memory::MEMORY_CAP,
                        if loaded.truncated { ", truncated" } else { "" }
                    );
                }
                for w in &loaded.warnings {
                    println!("warning: {}", crate::term::sanitize(w));
                }
                continue;
            }

            // /model: セッション中に provider / model を切り替える (#81)。台帳は引き継ぐ。
            if head == "model" {
                if args.is_empty() {
                    let active = cfg.llm.active();
                    println!(
                        "model: {}:{}",
                        cfg.llm.provider.as_str(),
                        crate::term::sanitize(&active.model)
                    );
                    for (p, c) in cfg.llm.configured() {
                        let marker = if p == cfg.llm.provider { "*" } else { " " };
                        println!(
                            "  {marker} {}:{}",
                            p.as_str(),
                            crate::term::sanitize(&c.model)
                        );
                    }
                    println!(
                        "usage: /model <provider> | /model <provider>:<model> | /model <model>"
                    );
                    continue;
                }
                let Some((provider, model)) = parse_model_arg(args, cfg.llm.provider) else {
                    println!(
                        "model: nothing to switch to (usage: /model <provider> | /model <provider>:<model> | /model <model>)"
                    );
                    continue;
                };
                let mut next = cfg.clone();
                next.llm.provider = provider;
                if let Some(m) = model {
                    next.llm.get_mut(provider).model = m;
                }
                match crate::llm::build_metered_with(&next, &runtime.ledger) {
                    Ok(client) => {
                        llm_client = client;
                        cfg = next;
                        session.switch_llm(cfg.llm.clone());
                        println!(
                            "model: now {}:{}",
                            cfg.llm.provider.as_str(),
                            crate::term::sanitize(&cfg.llm.active().model)
                        );
                    }
                    Err(e) => println!(
                        "model: not switched: {}",
                        crate::term::sanitize(&format!("{e:#}"))
                    ),
                }
                continue;
            }

            // /status: いま何で動いているか (#81)。
            if head == "status" {
                let active = cfg.llm.active();
                let shown = cfg.redacted();
                println!(
                    "provider: {}  model: {}",
                    cfg.llm.provider.as_str(),
                    crate::term::sanitize(&active.model)
                );
                println!(
                    "base_url: {}",
                    crate::term::sanitize(&shown.llm.active().base_url)
                );
                let used = session.usage().last_context_tokens;
                match (used * 100).checked_div(active.context_window) {
                    Some(pct) => println!(
                        "context: {used} / {} tokens ({pct}%; auto-compact at {}%)",
                        active.context_window, cfg.agent.auto_compact_percent
                    ),
                    None => println!("context: {used} tokens (window unknown)"),
                }
                println!(
                    "mode: {}  permissions: {}  sandbox: {}",
                    match session.mode() {
                        agent::Mode::Plan => "plan",
                        agent::Mode::Normal => "normal",
                    },
                    cfg.permissions.mode.as_str(),
                    cfg.sandbox.mode.as_str()
                );
                println!("cwd: {}", runtime.cwd.display());
                println!(
                    "session: {}",
                    recorder.as_ref().map_or("(not saved)", |r| r.id())
                );
                let deferred = registry.deferred_names().len();
                println!(
                    "tools: {} of {} visible{}  hooks: {}  mcp prompts: {}",
                    registry.len(),
                    registry.registered_len(),
                    if deferred > 0 {
                        format!(" (+{deferred} loadable with ToolSearch)")
                    } else {
                        String::new()
                    },
                    cfg.hooks.len(),
                    mcp_prompts.len()
                );
                continue;
            }

            // /context: コンテキストの内訳 (#81)。
            if head == "context" {
                println!("{}", session.context_breakdown().describe());
                continue;
            }

            // /undo は直近ターンのファイル変更を巻き戻す (session を要する)。
            if head == "undo" {
                match session.undo_last_turn() {
                    // パスはモデルが選んだもの。
                    Some(report) => println!("{}", crate::term::sanitize(&report.describe())),
                    None => println!("nothing to undo (no recorded file changes)"),
                }
                continue;
            }

            // /plan・/accept はプランモードの切替 (session を要する)。
            if head == "plan" {
                match session.mode() {
                    agent::Mode::Plan => println!("already in plan mode (/accept to leave)"),
                    agent::Mode::Normal => {
                        session.set_mode(agent::Mode::Plan);
                        println!(
                            "{}",
                            crate::term::dim(
                                "[plan] entered plan mode — destructive tools are disabled. \
                                 Investigate & plan, then /accept to approve and execute"
                            )
                        );
                    }
                }
                continue;
            }
            if head == "accept" {
                match session.mode() {
                    agent::Mode::Plan => {
                        session.set_mode(agent::Mode::Normal);
                        println!(
                            "{}",
                            crate::term::dim(
                                "[plan] accepted — back to normal mode, destructive tools re-enabled"
                            )
                        );
                    }
                    agent::Mode::Normal => println!("not in plan mode (/plan to enter)"),
                }
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
                let evaluator = match &runtime.goal_evaluator {
                    Some((client, model)) => crate::goal::Evaluator {
                        llm: client.as_ref(),
                        model,
                    },
                    None => crate::goal::Evaluator {
                        llm: llm_client.as_ref(),
                        model: &cfg.llm.active().model,
                    },
                };
                handle_goal(
                    args,
                    &mut goal_state,
                    &mut session,
                    llm_client.as_ref(),
                    evaluator,
                    &gate,
                    &mut recorder,
                )
                .await;
                continue;
            }

            match handle_slash(head, &registry, &user_commands, mcp_prompts) {
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
                            // MCP サーバの文言が入る。
                            Err(e) => eprintln!(
                                "{}",
                                crate::term::red_err(&format!("mcp prompt /{head} failed: {e:#}"))
                            ),
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
    let _ = session
        .fire_hook(Lifecycle::SessionEnd, None, serde_json::json!({}))
        .await;

    Ok(())
}

/// `/goal` builtin。引数なし = 状態表示、解除別名 = 解除、それ以外 = 条件設定＋
/// 達成までの自律ループ開始。破壊的ツールは既存の承認ゲートをそのまま通る
/// (自動承認したいときは `--yes` / `auto_approve`)。
async fn handle_goal(
    args: &str,
    goal_state: &mut Option<crate::goal::Goal>,
    session: &mut agent::Session,
    llm: &dyn llm::LlmClient,
    evaluator: crate::goal::Evaluator<'_>,
    gate: &PermissionGate,
    recorder: &mut Option<Recorder>,
) {
    run_goal_command(args, goal_state, session, llm, evaluator, gate, recorder).await;
    // どの経路で抜けても、残った状態 (paused の goal、または「無い」) をセッションに残す。
    save_goal_state(recorder.as_ref(), goal_state.as_ref());
}

/// goal の状態をセッションに保存する。失敗しても作業は止めない。
fn save_goal_state(recorder: Option<&Recorder>, goal: Option<&crate::goal::Goal>) {
    if let Some(rec) = recorder
        && let Err(e) = rec.save_goal(goal.map(crate::goal::Goal::to_record).as_ref())
    {
        eprintln!("session: could not save the goal: {e:#}");
    }
}

async fn run_goal_command(
    args: &str,
    goal_state: &mut Option<crate::goal::Goal>,
    session: &mut agent::Session,
    llm: &dyn llm::LlmClient,
    evaluator: crate::goal::Evaluator<'_>,
    gate: &PermissionGate,
    recorder: &mut Option<Recorder>,
) {
    use crate::goal::{Goal, GoalOutcome};

    // 状態表示
    if args.is_empty() {
        match goal_state {
            // 条件文は保存されたファイルから戻ってくることもある。
            Some(g) => println!("goal (paused):\n{}", crate::term::sanitize(&g.describe())),
            None => println!("no active goal — set one with /goal <condition>"),
        }
        return;
    }

    // 解除
    if GOAL_CLEAR_ALIASES.contains(&args) {
        match goal_state.take() {
            Some(g) => println!(
                "goal cleared: {}",
                crate::term::sanitize(&first_line(&g.condition))
            ),
            None => println!("no active goal to clear"),
        }
        return;
    }

    // 再開: paused の goal を、新しい上限の枠で続きから走らせる。
    let mut goal = if GOAL_RESUME_ALIASES.contains(&args) {
        let Some(mut paused) = goal_state.take() else {
            println!("no paused goal to resume — set one with /goal <condition>");
            return;
        };
        paused.begin_new_window();
        println!(
            "{}",
            crate::term::dim(&format!(
                "[goal] resumed after {} turn(s) (limits for this run: {} turns / {}s): {}",
                paused.total_turns,
                paused.max_turns,
                paused.max_duration.as_secs(),
                crate::term::sanitize(&first_line(&paused.condition))
            ))
        );
        paused
    } else {
        // 設定＋実行
        let goal = match Goal::new(args) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", crate::term::red_err(&shown_error("goal", &e)));
                return;
            }
        };
        if let Some(old) = goal_state.take() {
            println!(
                "goal replaced: {}",
                crate::term::sanitize(&first_line(&old.condition))
            );
        }
        println!(
            "{}",
            crate::term::dim(&format!(
                "[goal] started (limits: {} turns / {}s). destructive tools still ask for approval unless --yes",
                goal.max_turns,
                goal.max_duration.as_secs()
            ))
        );
        goal
    };

    // Ctrl-C で自律ループごと中断できるようにする (ターン途中なら履歴を修復)。
    let outcome = {
        let fut = crate::goal::drive_with(&mut goal, session, llm, evaluator, gate, |s, g| {
            if let Some(rec) = recorder.as_mut()
                && let Err(e) = rec.sync(s.history())
            {
                eprintln!("session: save failed: {e}");
            }
            // 途中で落ちても、ここまでの goal が paused として残るように。
            save_goal_state(recorder.as_ref(), Some(g));
        });
        tokio::pin!(fut);
        tokio::select! {
            out = &mut fut => Some(out),
            _ = wait_ctrl_c() => None,
        }
    };
    let Some(outcome) = outcome else {
        // 中断されると `drive_with` の後始末を通らないので、ここで時計を止める。
        goal.pause();
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
                    "[goal] achieved after {turns} turn(s): {}",
                    crate::term::sanitize(&reason)
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
            eprintln!("{}", crate::term::red_err(&shown_error("loop", &e)));
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
                "[loop] stopped: turn failed after {iterations} completed iteration(s): {}",
                crate::term::sanitize(&format!("{error:#}"))
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
                    eprintln!("{}", crate::term::red_err(&shown_error("error", &e)));
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

/// 端末に出すエラー文。エラーにはプロバイダの応答本文やツールの出力が入り込む
/// (`LLM HTTP 500: <body>`)。`base_url` は任意に設定できるので、本文は外から来る文字列。
fn shown_error(label: &str, e: &anyhow::Error) -> String {
    // 無害化は `term::red_err` が行う。
    format!("{label}: {e:#}")
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

/// プロンプト文字列。`[ui] prompt_status = true` で tty のときだけ、モデル名とコンテキスト使用率を
/// 添える (#81)。パイプには装飾を出さない。
fn prompt_line(cfg: &Config, session: &agent::Session) -> String {
    let base = match session.mode() {
        agent::Mode::Plan => "lodan (plan)",
        agent::Mode::Normal => "lodan",
    };
    if !cfg.ui.prompt_status || !crate::term::is_terminal() {
        return format!("{base}> ");
    }
    let active = cfg.llm.active();
    let used = session.usage().last_context_tokens;
    let ctx = (used * 100)
        .checked_div(active.context_window)
        .map_or_else(|| format!("{used} tok"), |pct| format!("ctx {pct}%"));
    format!(
        "{base} [{} · {ctx}]> ",
        crate::term::sanitize(&active.model)
    )
}

/// `!<cmd>` の実行。`sh -c` で cwd から、Bash ツールと同じ timeout。出力は stdout + stderr を
/// 順に並べ、長すぎる分は切る (文脈として添えるものなので)。
async fn run_shell_for_user(cmd: &str, cwd: &std::path::Path, timeout_secs: u64) -> String {
    const MAX_BYTES: usize = 16 * 1024;
    let run = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return format!("(failed to start: {e})"),
        Err(_) => return format!("(timed out after {timeout_secs}s)"),
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if !out.status.success() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("(exit {})", out.status.code().unwrap_or(-1)));
    }
    if text.len() > MAX_BYTES {
        let cut = text
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= MAX_BYTES)
            .last()
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… (truncated)");
    }
    text
}

/// `/model` の引数。provider 名そのもの / `provider:model` / モデル名 (コロンを含んでよい —
/// ollama の `qwen2.5-coder:7b` が local の標準)。`:` の前が provider として読めるときだけ
/// provider:model と解釈し、それ以外は全体をいまの provider のモデル名にする。
fn parse_model_arg(
    args: &str,
    current: crate::config::Provider,
) -> Option<(crate::config::Provider, Option<String>)> {
    let parse =
        |s: &str| <crate::config::Provider as clap::ValueEnum>::from_str(s.trim(), true).ok();
    let args = args.trim();
    if args.is_empty() || args == ":" {
        return None;
    }
    if let Some(p) = parse(args) {
        return Some((p, None));
    }
    if let Some((head, rest)) = args.split_once(':')
        && let Some(p) = parse(head)
    {
        let rest = rest.trim();
        return Some((p, (!rest.is_empty()).then(|| rest.to_string())));
    }
    Some((current, Some(args.to_string())))
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
                    "/model [provider[:model]]",
                    "セッション中にモデルを切り替え / 引数なしで現在値と設定済み provider",
                ),
                (
                    "/status",
                    "provider / model / コンテキスト使用率 / モード / cwd / session",
                ),
                (
                    "/context",
                    "コンテキストの内訳 (system / tools / user / assistant / tool)",
                ),
                ("/memory", "読み込まれているメモリファイルの一覧とサイズ"),
                ("!<cmd>", "シェルで実行して、出力を次の発話の文脈に添える"),
                (
                    "/goal <条件> | /goal | /goal clear",
                    "条件達成までターンを自律継続 / 状態表示 / 解除",
                ),
                (
                    "/loop <間隔> <プロンプト|/cmd>",
                    "固定間隔で反復実行 (5s/5m/2h/1d、Ctrl-C で停止)",
                ),
                (
                    "/plan | /accept",
                    "プランモード進入 (read-only 調査・計画のみ) / 承認して通常モードへ",
                ),
                (
                    "/undo",
                    "直近ターンのファイル変更を巻き戻す (Bash の副作用は対象外)",
                ),
            ] {
                println!("  {} — {desc}", crate::term::cyan(name));
            }
            if !user_commands.is_empty() {
                println!("{}", crate::term::bold("user commands:"));
                for c in user_commands.values() {
                    // 名前はプロジェクトのファイル名由来。
                    let name = crate::term::cyan(&format!("/{}", crate::term::sanitize(&c.name)));
                    if c.description.is_empty() {
                        println!("  {name}");
                    } else {
                        println!("  {name} — {}", crate::term::sanitize(&c.description));
                    }
                }
            }
            if !mcp_prompts.is_empty() {
                println!("{}", crate::term::bold("mcp prompts:"));
                for p in mcp_prompts.values() {
                    // MCP サーバが名乗った名前と説明。
                    let name =
                        crate::term::cyan(&format!("/{}", crate::term::sanitize(p.full_name())));
                    if p.description().is_empty() {
                        println!("  {name}");
                    } else {
                        println!("  {name} — {}", crate::term::sanitize(p.description()));
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
                // MCP ツールの名前と説明はサーバが決める。
                let desc = first_line(spec.function.description);
                println!(
                    "{} — {}",
                    crate::term::cyan(&crate::term::sanitize(spec.function.name)),
                    crate::term::sanitize(&desc)
                );
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
            eprintln!(
                "slash: /{} shadows a builtin, skipped",
                crate::term::sanitize(&cmd.name)
            );
            continue;
        }
        map.insert(cmd.name.clone(), cmd);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::parse_model_arg;
    use super::prompt_line;

    /// テストの stdout は tty ではないので、`prompt_status = true` でも装飾は付かない。
    #[test]
    fn prompt_has_no_decoration_off_a_tty() {
        let mut cfg = crate::config::Config::default();
        cfg.ui.prompt_status = true;
        let session = crate::agent::Session::new(
            cfg.clone(),
            std::sync::Arc::new(crate::tools::registry::default_registry()),
        );
        assert_eq!(prompt_line(&cfg, &session), "lodan> ");
    }

    #[test]
    fn model_arg_keeps_colons_inside_model_names() {
        use crate::config::Provider;
        let p = |s: &str| parse_model_arg(s, Provider::Local);
        assert_eq!(
            p("qwen2.5-coder:7b"),
            Some((Provider::Local, Some("qwen2.5-coder:7b".into())))
        );
        assert_eq!(p("kimi"), Some((Provider::Kimi, None)));
        assert_eq!(p("LOCAL"), Some((Provider::Local, None)));
        assert_eq!(
            p("local:qwen3.5:9b"),
            Some((Provider::Local, Some("qwen3.5:9b".into())))
        );
        assert_eq!(p("sakana:"), Some((Provider::Sakana, None)));
        assert_eq!(p(":"), None);
        assert_eq!(p(""), None);
        assert_eq!(
            parse_model_arg("gpt-oss:20b", Provider::Kimi),
            Some((Provider::Kimi, Some("gpt-oss:20b".into())))
        );
    }

    use super::looks_like_slash_command;
    use super::slash_candidates;

    fn cmds() -> Vec<String> {
        ["help", "goal", "loop", "plan", "review"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn slash_completion_matches_prefix() {
        let c = slash_candidates("/pl", 3, &cmds()).unwrap();
        assert_eq!(c, vec!["/plan"]);
        let c = slash_candidates("/", 1, &cmds()).unwrap();
        assert_eq!(c.len(), 5, "bare slash lists all commands");
    }

    #[test]
    fn slash_completion_only_in_first_token() {
        // 引数位置 (空白の後) はパス補完へフォールバックする。
        assert!(slash_candidates("/loop 5m /rev", 13, &cmds()).is_none());
        // slash で始まらない行は対象外。
        assert!(slash_candidates("hello", 5, &cmds()).is_none());
        assert!(slash_candidates("", 0, &cmds()).is_none());
    }

    #[test]
    fn multiline_fence_needs_more_until_closed() {
        use super::input_needs_more;
        assert!(input_needs_more("```"));
        assert!(input_needs_more("```python\nx = 1"));
        assert!(!input_needs_more("```python\nx = 1\n```"));
        assert!(!input_needs_more("```\ncode\n``` "), "trailing space ok");
    }

    #[test]
    fn multiline_backslash_continues() {
        use super::input_needs_more;
        assert!(input_needs_more("line1\\"));
        assert!(!input_needs_more("line1"));
        assert!(!input_needs_more("line1\\\nline2"));
    }

    #[test]
    fn normalize_strips_fence_and_joins_continuations() {
        use super::normalize_input;
        assert_eq!(
            normalize_input("```python\nx = 1\ny = 2\n```"),
            "x = 1\ny = 2"
        );
        assert_eq!(normalize_input("a\\\nb"), "a\nb");
        assert_eq!(normalize_input("plain single line"), "plain single line");
        // フェンス内の行末バックスラッシュはそのまま残る (中身は無加工)。
        assert_eq!(normalize_input("```\nkeep \\\n```"), "keep \\");
        // パイプ入力の単一行は末尾 \ も含め無加工 (pr-review #59)。
        assert_eq!(normalize_input("ls dir\\"), "ls dir\\");
    }

    #[test]
    fn slash_completion_respects_cursor_position() {
        // カーソルが `/pl` の直後にある場合のみその位置までで判定する。
        let c = slash_candidates("/pl 残りは無視", 3, &cmds()).unwrap();
        assert_eq!(c, vec!["/plan"]);
    }

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
