use anyhow::Result;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::llm;
use crate::mcp;
use crate::mcp::prompt::McpPrompt;
use crate::permission::PermissionGate;
use crate::session::Recorder;
use crate::slash::{self, SlashCommand};
use crate::tools::registry::default_registry;

/// REPL 組み込みコマンド。ユーザ定義コマンドより優先する。
const BUILTINS: &[&str] = &["exit", "quit", "help", "clear", "tools"];

pub async fn run(cfg: Config, resume: Option<String>) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    println!(
        "lodan {} — type /help for commands, /exit to quit",
        env!("CARGO_PKG_VERSION")
    );
    let active = cfg.llm.active();
    println!(
        "model: {} @ {} ({})",
        active.model,
        active.base_url,
        cfg.llm.provider.as_str()
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

            match handle_slash(head, &registry, &user_commands, &mcp_prompts) {
                SlashResult::Exit => break,
                SlashResult::Handled => continue,
                SlashResult::Unknown => {
                    // 組み込みに無ければユーザ定義コマンド → MCP prompt の順に試す。
                    if let Some(cmd) = user_commands.get(head) {
                        let prompt = slash::expand(&cmd.body, args);
                        if let Err(e) = session.run_turn(&prompt, llm_client.as_ref(), &gate).await
                        {
                            eprintln!("error: {e:#}");
                        }
                        persist(&mut recorder, &session);
                    } else if let Some(mcp_prompt) = mcp_prompts.get(head) {
                        let positional: Vec<&str> = args.split_whitespace().collect();
                        match mcp_prompt.render(&positional).await {
                            Ok(text) if !text.trim().is_empty() => {
                                if let Err(e) =
                                    session.run_turn(&text, llm_client.as_ref(), &gate).await
                                {
                                    eprintln!("error: {e:#}");
                                }
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

        if let Err(e) = session.run_turn(line, llm_client.as_ref(), &gate).await {
            eprintln!("error: {e:#}");
        }
        persist(&mut recorder, &session);
    }

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

fn handle_slash(
    cmd: &str,
    registry: &crate::tools::registry::ToolRegistry,
    user_commands: &BTreeMap<String, SlashCommand>,
    mcp_prompts: &BTreeMap<String, McpPrompt>,
) -> SlashResult {
    match cmd {
        "exit" | "quit" => SlashResult::Exit,
        "help" => {
            println!("/exit /clear /tools /help");
            for c in user_commands.values() {
                if c.description.is_empty() {
                    println!("/{}", c.name);
                } else {
                    println!("/{} — {}", c.name, c.description);
                }
            }
            for p in mcp_prompts.values() {
                if p.description().is_empty() {
                    println!("/{}", p.full_name());
                } else {
                    println!("/{} — {}", p.full_name(), p.description());
                }
            }
            SlashResult::Handled
        }
        "clear" => {
            print!("\x1b[2J\x1b[H");
            SlashResult::Handled
        }
        "tools" => {
            for name in registry.names() {
                println!("- {name}");
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
