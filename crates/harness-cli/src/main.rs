#![allow(clippy::manual_map, clippy::needless_return, clippy::ptr_arg)]
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use clap::Parser;

mod cmd;
mod commands;
mod gc;

fn main() -> anyhow::Result<()> {
    let cli = commands::Cli::parse();
    let exit_code = {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(commands::run(cli))?
    };
    if exit_code == 0 {
        Ok(())
    } else {
        std::process::exit(exit_code);
    }
}
