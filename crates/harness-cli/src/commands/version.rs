pub fn run() -> anyhow::Result<()> {
    let cargo_toml_path = std::env::current_dir()?.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        anyhow::bail!("Cargo.toml not found in current directory");
    }
    let content = std::fs::read_to_string(&cargo_toml_path)?;
    let parsed: toml::Value = toml::from_str(&content)?;

    if let Some(version) = parsed
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        println!("Current version: {}", version);
    } else if let Some(version) = parsed
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        println!("Current version: {}", version);
    } else {
        anyhow::bail!("Version field not found in Cargo.toml");
    }
    Ok(())
}
