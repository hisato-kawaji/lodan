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

    /// Append a machine-readable JSONL run log (turns, tool calls, timings) to PATH
    #[arg(long, env = "LODAN_LOG_JSONL", value_name = "PATH")]
    pub log_jsonl: Option<std::path::PathBuf>,

    /// Override sampling temperature for the active provider (0.1-0.2 steadies small local models)
    #[arg(long, env = "LODAN_TEMPERATURE")]
    pub temperature: Option<f32>,

    /// Nudge the model to self-verify once before finishing (#63)
    #[arg(long, env = "LODAN_FINISH_NUDGE", num_args = 0..=1, default_missing_value = "true")]
    pub finish_nudge: Option<bool>,

    /// Ask the model to re-issue tool calls that leaked as text (#61)
    #[arg(long, env = "LODAN_MALFORMED_RETRY", num_args = 0..=1, default_missing_value = "true")]
    pub malformed_retry: Option<bool>,

    /// Skip a read-only tool call identical to the immediately preceding one (#61)
    #[arg(long, env = "LODAN_DUP_SUPPRESS", num_args = 0..=1, default_missing_value = "true")]
    pub dup_suppress: Option<bool>,

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
    cfg.apply_overrides(crate::config::Overrides {
        provider: args.provider,
        base_url: args.base_url,
        model: args.model,
        api_key: args.api_key,
        temperature: args.temperature,
        auto_approve: args.yes,
        finish_nudge: args.finish_nudge,
        malformed_retry: args.malformed_retry,
        dup_suppress: args.dup_suppress,
    });

    // 計測が本編を壊さないよう、ログを開けなくても実行は続ける。
    if let Some(path) = args.log_jsonl.as_deref() {
        match crate::runlog::init(path) {
            Ok(()) => crate::runlog::record(
                "run_start",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "provider": cfg.llm.provider.as_str(),
                    "model": cfg.llm.active().model,
                    "cwd": std::env::current_dir().unwrap_or_default().display().to_string(),
                }),
            ),
            Err(e) => eprintln!("runlog: disabled ({e})"),
        }
    }

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
