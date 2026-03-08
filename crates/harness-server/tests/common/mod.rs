/// Create a temp directory under $HOME, falling back to the OS default
/// temp directory if $HOME is not set or not writable.
pub fn tempdir_in_home(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(dir) = tempfile::Builder::new().prefix(prefix).tempdir_in(home) {
            return Ok(dir);
        }
    }
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .map_err(Into::into)
}
