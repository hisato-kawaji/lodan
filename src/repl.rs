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
    println!("model: {} @ {}", cfg.llm.model, cfg.llm.base_url);

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

        if let Some(rest) = line.strip_prefix('/') {
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
