#![allow(clippy::manual_map, clippy::needless_return, clippy::ptr_arg)]
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use clap::Parser;

mod commands;

fn main() -> anyhow::Result<()> {
    let cli = commands::Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(commands::run(cli))
}
