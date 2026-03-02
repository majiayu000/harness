use clap::Parser;

mod cmd;
mod commands;
mod gc;

fn main() -> anyhow::Result<()> {
    let cli = commands::Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(commands::run(cli))
}
