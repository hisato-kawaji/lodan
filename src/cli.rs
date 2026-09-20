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

    /// Provider to fall back to when the primary is temporarily unavailable (5xx / 429 / connect errors after retries)
    #[arg(
        long,
        env = "LODAN_FALLBACK_PROVIDER",
        value_enum,
        value_name = "PROVIDER"
    )]
    pub fallback_provider: Option<Provider>,

    /// How calls that need approval are handled: default, accept-edits, plan, dont-ask, bypass (= --yes)
    #[arg(long, env = "LODAN_PERMISSION_MODE", value_enum)]
    pub permission_mode: Option<crate::config::PermissionMode>,

    /// Add an allow rule, e.g. --allowed-tools "Bash(git status)" (repeatable; see README for the syntax)
    #[arg(long = "allowed-tools", value_name = "RULE")]
    pub allowed_tools: Vec<String>,

    /// Add a deny rule, e.g. --disallowed-tools "Read(**/.env)" (repeatable; deny wins even with --yes)
    #[arg(long = "disallowed-tools", value_name = "RULE")]
    pub disallowed_tools: Vec<String>,

    /// Trust this directory's project settings for this run only (.lodan/, .mcp.json, LODAN.md / CLAUDE.md)
    #[arg(long, env = "LODAN_TRUST", value_parser = clap::builder::BoolishValueParser::new(), num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub trust: Option<bool>,

    /// Run consecutive parallel-safe tool calls (Read/Grep/Glob/WebFetch/WebSearch/Task) concurrently
    #[arg(long, env = "LODAN_PARALLEL_TOOLS", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub parallel_tools: Option<bool>,

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
    /// Trust the current directory's project settings (or list / remove trusted directories)
    Trust {
        /// List trusted directories
        #[arg(long, conflicts_with = "remove")]
        list: bool,
        /// Stop trusting the current directory
        #[arg(long)]
        remove: bool,
    },
}

/// 戻り値はプロセスの終了コード。
pub async fn dispatch(args: Cli) -> Result<i32> {
    if let Some(Command::Trust { list, remove }) = &args.cmd {
        return manage_trust(*list, *remove).map(|()| 0);
    }
    // プロジェクトのファイルを読むかどうかは、設定を読む前に決める (設定そのものが対象なので)。
    // バイナリでは main が `.env` を読む前に済ませている。ここは、それ以外の呼び出し元のための保険
    // (2 回目の決定は無視される)。
    decide_project_trust(&args);

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
        fallback: args.fallback_provider,
        base_url: args.base_url,
        model: args.model,
        api_key: args.api_key,
        temperature: args.temperature,
        auto_approve: args.yes,
        finish_nudge: args.finish_nudge,
        malformed_retry: args.malformed_retry,
        dup_suppress: args.dup_suppress,
        parallel_tools: args.parallel_tools,
        permission_mode: args.permission_mode,
        allowed_tools: args.allowed_tools,
        disallowed_tools: args.disallowed_tools,
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
        Command::Trust { .. } => unreachable!("handled before the config is loaded"),
    }
}

/// このプロセスがプロジェクトのファイルを読んでよいかを決めて固定する (#75)。
/// 尋ねるのは対話の REPL だけ。`-p` / `config` / パイプ入力では尋ねずに「読まない」側へ倒す。
pub fn decide_project_trust(args: &Cli) {
    use std::io::IsTerminal;
    // main が既に決めている。尋ね直すと、2 回目の答えはこのプロセスには効かないのに
    // `(y)` の記録だけが残る (1 回目に断った信頼が次回から効いてしまう)。
    if crate::trust::is_decided() {
        return;
    }
    // 信頼の管理と、プロジェクトのファイルを使わないサブコマンドでは尋ねない。読みもしない。
    if matches!(args.cmd, Some(Command::Trust { .. } | Command::Sessions)) {
        crate::trust::set_project_trusted(false);
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let store = crate::trust::store_path();
    let is_repl = args.print.is_none() && matches!(args.cmd, None | Some(Command::Repl));
    let request = crate::trust::Request {
        cwd: &cwd,
        store_path: store.as_deref(),
        trust_flag: args.trust.unwrap_or(false),
        interactive: is_repl && std::io::stdin().is_terminal(),
    };
    // 問い合わせも通知も stderr へ。`-p` の stdout は機械可読な契約。
    let trusted = crate::trust::decide(
        &request,
        &mut std::io::stdin().lock(),
        &mut std::io::stderr().lock(),
    );
    crate::trust::set_project_trusted(trusted);
}

fn manage_trust(list: bool, remove: bool) -> Result<()> {
    let store = crate::trust::store_path()
        .ok_or_else(|| anyhow::anyhow!("cannot locate the lodan config directory"))?;
    let cwd = std::env::current_dir()?;
    if list {
        for dir in crate::trust::list(&store)? {
            println!("{}", crate::trust::shown(&dir));
        }
    } else if remove {
        if crate::trust::forget(&store, &cwd)? {
            println!("no longer trusting {}", crate::trust::shown(&cwd));
        } else {
            println!(
                "{} was not trusted on its own (a parent directory may be; see `lodan trust --list`)",
                crate::trust::shown(&cwd)
            );
        }
    } else {
        crate::trust::record(&store, &cwd)?;
        println!(
            "trusting {} (and everything under it)",
            crate::trust::shown(&cwd)
        );
        // メモリは祖先のディレクトリからも読まれる。何が効くようになったかを、対話の確認と同じ一覧で見せる。
        let files = crate::trust::project_files(&cwd);
        if !files.is_empty() {
            println!("lodan will now read, when started here:");
            for file in files {
                println!("  {file}");
            }
        }
    }
    Ok(())
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
