use anyhow::Result;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::llm;
use crate::mcp;
use crate::permission::PermissionGate;
use crate::slash::{self, SlashCommand};
use crate::tools::registry::default_registry;

/// REPL 組み込みコマンド。ユーザ定義コマンドより優先する。
const BUILTINS: &[&str] = &["exit", "quit", "help", "clear", "tools"];

pub async fn run(cfg: Config) -> Result<()> {
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

    // skills::load_from(...)              // MVP 外

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let user_commands = load_user_commands(&cwd.join(".lodan/commands"));
    if !user_commands.is_empty() {
        println!("slash: {} user command(s) loaded", user_commands.len());
    }

    let mut registry = default_registry();
    let mcp_outcome = mcp::registry::load_and_register(&mut registry)
        .await
        .unwrap_or_else(|e| {
            eprintln!("mcp: {e}");
            mcp::registry::LoadOutcome::default()
        });
    if mcp_outcome.servers > 0 {
        println!(
            "mcp: {} server(s), {} tool(s) registered",
            mcp_outcome.servers, mcp_outcome.tools
        );
    }
    // Keep clients alive for the full session; Drop kills subprocesses.
    let _mcp_clients = mcp_outcome.clients;
    let registry = Arc::new(registry);
    let llm_client: Arc<dyn llm::LlmClient> = llm::build_client(&cfg)?;
    let gate = PermissionGate::new(cfg.agent.auto_approve);
    let mut session = agent::Session::new(cfg.clone(), Arc::clone(&registry));

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

            match handle_slash(head, &registry, &user_commands) {
                SlashResult::Exit => break,
                SlashResult::Handled => continue,
                SlashResult::Unknown => {
                    // 組み込みに無ければユーザ定義コマンドを試す。
                    if let Some(cmd) = user_commands.get(head) {
                        let prompt = slash::expand(&cmd.body, args);
                        if let Err(e) = session.run_turn(&prompt, llm_client.as_ref(), &gate).await
                        {
                            eprintln!("error: {e:#}");
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
    }

    Ok(())
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
