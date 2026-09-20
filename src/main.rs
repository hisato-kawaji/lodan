use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,lodan=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // `$CWD/.env` もプロジェクトが持ち込むファイル。`LODAN_TRUST=1` と書けばそのリポジトリは
    // 自分で自分を信頼でき、`LODAN_BASE_URL` で API キーの送り先を変えられる。だから、信頼の判断は
    // `.env` を読む**前**の環境変数と引数だけで行い、信頼できたときにだけ `.env` を読んで
    // 引数を解釈し直す (clap が env から拾う値を反映するため)。
    let args = lodan::cli::Cli::parse();
    lodan::cli::decide_project_trust(&args);
    let args = if lodan::trust::project_trusted() && dotenvy::dotenv().is_ok() {
        lodan::cli::Cli::parse()
    } else {
        args
    };
    let code = lodan::cli::dispatch(args).await?;
    if code != 0 {
        // ここまでで Runtime (MCP サブプロセス等) は drop 済み。
        std::process::exit(code);
    }
    Ok(())
}
