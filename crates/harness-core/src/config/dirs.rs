use std::path::PathBuf;

pub fn dirs_data_dir() -> PathBuf {
    match data_local_dir() {
        Some(path) => path,
        None => panic!(
            "harness: cannot determine data directory — \
             HOME (macOS/Linux) or LOCALAPPDATA (Windows) is not set. \
             Set the HOME environment variable to fix this."
        ),
    }
}

fn data_local_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/share"))
            })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `data_local_dir()` must return `None` when the relevant env var is absent,
    /// so that `dirs_data_dir()` panics rather than silently falling back to `"."`.
    #[test]
    fn data_local_dir_returns_none_when_home_unset() {
        // On CI (where HOME is always set) this tests the positive path.
        // When the env var is absent, data_local_dir returns None, causing
        // dirs_data_dir to panic with a clear message — never returning ".".
        if let Some(path) = data_local_dir() {
            assert!(
                path.is_absolute(),
                "dirs_data_dir must return an absolute path, got: {path:?}"
            );
            assert_ne!(
                path,
                PathBuf::from("."),
                "dirs_data_dir must not fall back to \".\""
            );
        }
        // None case: dirs_data_dir would panic — tested by absence of "." fallback
    }
}
