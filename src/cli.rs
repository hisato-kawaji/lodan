use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{Config, Provider};
use crate::repl;

#[derive(Debug, Parser)]
#[command(
    name = "lodan",
    version,
    about = "Local-LLM coding agent (Claude Code inspired)"
)]
pub struct Cli {
    /// Select LLM provider
    #[arg(long, env = "LODAN_PROVIDER", value_enum)]
    pub provider: Option<Provider>,

    /// Override base URL for the active provider (OpenAI-compatible)
    #[arg(long, env = "LODAN_BASE_URL")]
    pub base_url: Option<String>,

    /// Override model name for the active provider
    #[arg(long, env = "LODAN_MODEL")]
    pub model: Option<String>,

    /// Override API key for the active provider (sent as Bearer if non-empty)
    #[arg(long, env = "LODAN_API_KEY")]
    pub api_key: Option<String>,

    /// Path to a config file
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,

    /// Auto-approve all destructive tool calls (CI / scripting)
    #[arg(long, env = "LODAN_AUTO_APPROVE")]
    pub yes: bool,

    /// Resume a saved session by id (or `last` for the most recent)
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print effective configuration
    Config,
    /// Start the interactive REPL (default if omitted)
    Repl,
    /// List saved sessions
    Sessions,
}

pub async fn dispatch(args: Cli) -> Result<()> {
    let mut cfg = Config::load(args.config.as_deref())?;
    cfg.apply_overrides(
        args.provider,
        args.base_url,
        args.model,
        args.api_key,
        args.yes,
    );

    match args.cmd.unwrap_or(Command::Repl) {
        Command::Repl => repl::run(cfg, args.resume).await,
        Command::Config => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
        Command::Sessions => list_sessions(),
    }
}

fn list_sessions() -> Result<()> {
    let sessions = crate::session::list_sessions()?;
    if sessions.is_empty() {
        println!("no saved sessions");
        return Ok(());
    }
    for meta in sessions {
        println!(
            "{}  {} ({})  {}",
            meta.id, meta.model, meta.provider, meta.cwd
        );
    }
    Ok(())
}
