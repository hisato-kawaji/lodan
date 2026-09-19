use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env from CWD before clap reads env-backed flags.
    // Absence is fine — users may rely on shell exports instead.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,lodan=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = lodan::cli::Cli::parse();
    let code = lodan::cli::dispatch(args).await?;
    if code != 0 {
        // ここまでで Runtime (MCP サブプロセス等) は drop 済み。
        std::process::exit(code);
    }
    Ok(())
}
