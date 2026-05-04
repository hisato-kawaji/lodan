use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::repl;

#[derive(Debug, Parser)]
#[command(
    name = "lodan",
    version,
    about = "Local-LLM coding agent (Claude Code inspired)"
)]
pub struct Cli {
    /// Override LLM base URL (OpenAI-compatible)
    #[arg(long, env = "LODAN_BASE_URL")]
    pub base_url: Option<String>,

    /// Override model name
    #[arg(long, env = "LODAN_MODEL")]
    pub model: Option<String>,

    /// Override API key (sent as Bearer if non-empty)
    #[arg(long, env = "LODAN_API_KEY")]
    pub api_key: Option<String>,

    /// Path to a config file
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,

    /// Auto-approve all destructive tool calls (CI / scripting)
    #[arg(long, env = "LODAN_AUTO_APPROVE")]
    pub yes: bool,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print effective configuration
    Config,
    /// Start the interactive REPL (default if omitted)
    Repl,
    // /// Manage MCP servers (out of MVP scope)
    // Mcp,
    // /// Manage plugins (out of MVP scope)
    // Plugin,
}

pub async fn dispatch(args: Cli) -> Result<()> {
    let mut cfg = Config::load(args.config.as_deref())?;
    cfg.apply_overrides(args.base_url, args.model, args.api_key, args.yes);

    match args.cmd.unwrap_or(Command::Repl) {
        Command::Repl => repl::run(cfg).await,
        Command::Config => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}
