#![allow(dead_code)]

mod agent;
mod cli;
mod config;
mod hooks;
mod llm;
mod mcp;
mod permission;
mod prompt;
mod repl;
mod session;
mod skills;
mod slash;
mod tools;

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

    let args = cli::Cli::parse();
    cli::dispatch(args).await
}
