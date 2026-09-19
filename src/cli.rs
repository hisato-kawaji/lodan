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
    #[arg(long, env = "LODAN_FINISH_NUDGE", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub finish_nudge: Option<bool>,

    /// Ask the model to re-issue tool calls that leaked as text (#61)
    #[arg(long, env = "LODAN_MALFORMED_RETRY", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub malformed_retry: Option<bool>,

    /// Skip a read-only tool call identical to the immediately preceding one (#61)
    #[arg(long, env = "LODAN_DUP_SUPPRESS", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub dup_suppress: Option<bool>,

    /// Which tools the model sees: full (default), core (Read/Write/Edit/Bash/Grep/Glob), readonly
    #[arg(long, env = "LODAN_TOOL_PROFILE", value_enum)]
    pub tool_profile: Option<crate::config::ToolProfile>,

    /// Exact list of tools the model sees, comma-separated (overrides --tool-profile)
    #[arg(
        long,
        env = "LODAN_TOOLS",
        value_delimiter = ',',
        value_name = "NAME,..."
    )]
    pub tools: Option<Vec<String>>,

    /// Run one turn non-interactively and exit. Without PROMPT, the prompt is read from stdin
    #[arg(short = 'p', long = "print", value_name = "PROMPT", num_args = 0..=1, default_missing_value = "")]
    pub print: Option<String>,

    /// With -p PROMPT: also read stdin to EOF and append it (`cat log | lodan -p "summarize" --stdin`)
    #[arg(long, requires = "print")]
    pub stdin: bool,

    /// What -p writes to stdout: the final answer, one JSON result, or JSONL events
    #[arg(long, value_enum, default_value_t, requires = "print")]
    pub output_format: crate::headless::OutputFormat,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print effective configuration
    Config {
        /// Also list which config file each explicitly-set key came from
        #[arg(long)]
        show_origin: bool,
    },
    /// Start the interactive REPL (default if omitted)
    Repl,
    /// List saved sessions
    Sessions,
}

/// 戻り値はプロセスの終了コード。
pub async fn dispatch(args: Cli) -> Result<i32> {
    let headless_format = args.print.is_some().then_some(args.output_format);
    let stream_json = headless_format == Some(crate::headless::OutputFormat::StreamJson);

    let loaded = Config::load_with_origins(args.config.as_deref());
    let (mut cfg, mut origins) = match (loaded, headless_format) {
        (Ok(loaded), _) => loaded,
        // ヘッドレスでは設定の読み込み失敗も、頼まれた形式の結果として stdout に出す。
        (Err(e), Some(format)) => {
            // 設定が読めていないので provider / model は分からない。それでもイベント列は
            // run_start から始め、`--log-jsonl` にも同じものを残す。
            init_runlog(args.log_jsonl.as_deref(), stream_json, None);
            return Ok(crate::headless::report_startup_failure(format, &e));
        }
        (Err(e), None) => return Err(e),
    };
    let overrides = crate::config::Overrides {
        provider: args.provider,
        base_url: args.base_url,
        model: args.model,
        api_key: args.api_key,
        temperature: args.temperature,
        auto_approve: args.yes,
        finish_nudge: args.finish_nudge,
        malformed_retry: args.malformed_retry,
        dup_suppress: args.dup_suppress,
        tool_profile: args.tool_profile,
        tools: args.tools,
    };
    cfg.apply_overrides_tracked(overrides, &mut origins);

    init_runlog(args.log_jsonl.as_deref(), stream_json, Some(&cfg));

    if let Some(prompt) = args.print {
        let format = args.output_format;
        if args.cmd.is_some() {
            let e = anyhow::anyhow!("-p cannot be combined with a subcommand");
            return Ok(crate::headless::report_startup_failure(format, &e));
        }
        let opts = crate::headless::Options {
            prompt,
            read_stdin: args.stdin,
            format,
            resume: args.resume,
        };
        return Ok(crate::headless::run(cfg, opts).await);
    }

    match args.cmd.unwrap_or(Command::Repl) {
        Command::Repl => repl::run(cfg, args.resume).await.map(|()| 0),
        Command::Config { show_origin } => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            if show_origin {
                print!("{}", describe_origins(&origins));
            }
            Ok(0)
        }
        Command::Sessions => list_sessions().map(|()| 0),
    }
}

/// runlog の sink を立てて `run_start` を記録する。計測が本編を壊さないよう、ログファイルを
/// 開けなくても実行は続ける。`cfg` が無いのは設定の読み込みに失敗した起動失敗の経路。
fn init_runlog(path: Option<&std::path::Path>, stream_json: bool, cfg: Option<&Config>) {
    if path.is_none() && !stream_json {
        return;
    }
    if let Err(e) = crate::runlog::init_with(path, stream_json) {
        eprintln!("runlog: disabled ({e})");
        // ログファイルを開けなくても、stream-json の stdout は契約なので生かす。
        if stream_json {
            let _ = crate::runlog::init_with(None, true);
        }
    }
    // どの経路で sink が立っても、イベント列は run_start から始まる。
    crate::runlog::record(
        "run_start",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "provider": cfg.map(|c| c.llm.provider.as_str()),
            "model": cfg.map(|c| c.llm.active().model.as_str()),
            "cwd": std::env::current_dir().unwrap_or_default().display().to_string(),
        }),
    );
}

/// `--show-origin` の表示。設定ファイル・env・CLI フラグのいずれかが決めたキーが載る。
/// ここに無い値は既定値。
fn describe_origins(origins: &crate::config::Origins) -> String {
    let mut out = String::from("# origins (keys not listed: built-in default)\n");
    if origins.is_empty() {
        out.push_str("# (nothing overrides the defaults)\n");
    }
    for (key, origin) in origins {
        out.push_str(&format!("# {key} <- {origin}\n"));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bool_flag_without_value_does_not_swallow_subcommand() {
        let cli = Cli::try_parse_from(["lodan", "--finish-nudge", "repl"]).unwrap();
        assert_eq!(cli.finish_nudge, Some(true));
        assert!(matches!(cli.cmd, Some(Command::Repl)));
    }

    #[test]
    fn bool_flag_takes_explicit_value_with_equals() {
        let cli =
            Cli::try_parse_from(["lodan", "--dup-suppress=false", "--malformed-retry=1"]).unwrap();
        assert_eq!(cli.dup_suppress, Some(false));
        assert_eq!(cli.malformed_retry, Some(true));
        assert_eq!(cli.finish_nudge, None);
    }
}
