use std::path::PathBuf;

pub fn dirs_data_dir() -> PathBuf {
    match data_local_dir() {
        Some(path) => path,
        // HOME / LOCALAPPDATA is absent — common in containers and systemd
        // services without `User=`. Fall back to a deterministic temp path so
        // startup succeeds. Production deployments should set an explicit
        // `data_dir` in their config rather than relying on this default.
        None => std::env::temp_dir().join("harness"),
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

    /// `dirs_data_dir()` must never return `"."` and must never panic,
    /// even when platform env vars (HOME / LOCALAPPDATA) are absent.
    #[test]
    fn data_local_dir_returns_absolute_path_or_temp_fallback() {
        let path = super::dirs_data_dir();
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
}
