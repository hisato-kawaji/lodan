use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::llm;
use crate::permission::PermissionGate;
use crate::tools::registry::default_registry;

pub async fn run(cfg: Config) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    println!("lodan {} — type /help for commands, /exit to quit", env!("CARGO_PKG_VERSION"));
    let active = cfg.llm.active();
    println!(
        "model: {} @ {} ({})",
        active.model,
        active.base_url,
        cfg.llm.provider.as_str()
    );

    // skills::load_from(...)              // MVP 外
    // slash::register_user_commands(...)  // MVP 外
    // mcp::registry::start_configured(...) // MVP 外

    let registry = Arc::new(default_registry());
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

        if let Some(rest) = line.strip_prefix('/').filter(|r| looks_like_slash_command(r)) {
            match handle_slash(rest, &registry) {
                SlashResult::Exit => break,
                SlashResult::Handled => continue,
                SlashResult::Unknown => {
                    eprintln!("unknown command: /{rest}");
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

fn handle_slash(cmd: &str, registry: &crate::tools::registry::ToolRegistry) -> SlashResult {
    match cmd {
        "exit" | "quit" => SlashResult::Exit,
        "help" => {
            println!("/exit /clear /tools /help");
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
        assert!(!looks_like_slash_command("tmp/lodan-demo/hello.txt に hi と書いて"));
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
