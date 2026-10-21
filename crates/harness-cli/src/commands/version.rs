pub fn run() -> anyhow::Result<()> {
    println!("Current version: {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
