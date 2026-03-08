use std::path::PathBuf;

/// Create a temp directory under a writable base path without mutating
/// global state (`HOME` env var).  Tries `$HOME` first; falls back to
/// `$CWD/.harness-test-home` if `$HOME` is not writable.
pub fn tempdir_in_home(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("resolve cwd"));
    if let Ok(dir) = tempfile::Builder::new().prefix(prefix).tempdir_in(&home) {
        return Ok(dir);
    }
    let fallback = std::env::current_dir()?.join(".harness-test-home");
    std::fs::create_dir_all(&fallback)?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&fallback)
        .map_err(Into::into)
}
