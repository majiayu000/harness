#![allow(clippy::manual_map, clippy::needless_return, clippy::ptr_arg)]

use clap::Parser;

mod cmd;
mod commands;
mod gc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness=info,warn".into()),
        )
        .init();

    let cli = commands::Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(commands::run(cli))
}
