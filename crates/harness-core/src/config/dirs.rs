use std::path::{Path, PathBuf};

pub fn dirs_data_dir() -> PathBuf {
    match data_local_dir() {
        Some(path) => path,
        // HOME / LOCALAPPDATA is absent — common in containers and systemd
        // services without `User=`. Fall back to a per-user temp path so
        // startup succeeds. The username is included to provide per-user
        // isolation: separate users or service accounts running harness
        // without HOME get distinct directories, preventing cross-instance
        // state collisions and reducing /tmp symlink hijacking risk.
        //
        // Production deployments should always set an explicit `data_dir`
        // in their config rather than relying on this default.
        None => temp_fallback_dir(),
    }
}

fn temp_fallback_dir() -> PathBuf {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string());
    // Keep the directory name filesystem-safe: allow alphanumeric, '-', '_' only.
    let safe: String = username
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("harness-{safe}"))
}

/// Return the canonical path for a named SQLite database file within `dir`.
///
/// All stores must construct their paths through this function so that the
/// file-name convention (`{name}.db`) is enforced in a single place and
/// divergence between callers is impossible.
pub fn default_db_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.db"))
}

/// Probe OS-standard config locations and return the first that exists as a file.
///
/// Search order:
/// 1. `$XDG_CONFIG_HOME/harness/config.toml` (or `~/.config/harness/config.toml`)
/// 2. macOS: `$HOME/Library/Application Support/harness/config.toml`
/// 3. Windows: `%APPDATA%\harness\config.toml`
///
/// Returns `None` if none of the candidates exist as a regular file.
pub fn find_config_file() -> Option<PathBuf> {
    for candidate in config_candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn config_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Reject relative env-var values: accepting them would make harness probe
    // paths relative to the process CWD, turning any directory an operator
    // starts harness from into a config boundary and potentially loading
    // attacker-controlled or accidental local config files.
    let abs_home: Option<PathBuf> = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());

    // 1. $XDG_CONFIG_HOME/harness/config.toml
    //    Per the XDG Base Directory Specification, XDG_CONFIG_HOME MUST be an
    //    absolute path. Relative values are silently discarded.
    if let Some(xdg) = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| abs_home.as_ref().map(|h| h.join(".config")))
    {
        candidates.push(xdg.join("harness").join("config.toml"));
    }

    // 2. macOS: $HOME/Library/Application Support/harness/config.toml
    #[cfg(target_os = "macos")]
    if let Some(home) = &abs_home {
        candidates.push(
            home.join("Library/Application Support")
                .join("harness")
                .join("config.toml"),
        );
    }

    // 3. Windows: %APPDATA%\harness\config.toml
    //    APPDATA must be absolute for the same reason as XDG_CONFIG_HOME.
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        candidates.push(appdata.join("harness").join("config.toml"));
    }

    candidates
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
    use std::fs;

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

    // Serialize env-var tests to prevent races when cargo runs tests in parallel.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env_vars<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save originals.
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            // SAFETY: test-only, serialized by ENV_MUTEX.
            unsafe { std::env::set_var(k, v) };
        }
        f();
        // Restore.
        for (k, orig) in &saved {
            match orig {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    /// When no config paths exist, `find_config_file` returns `None`.
    #[test]
    fn find_config_file_returns_none_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_str().unwrap().to_owned();
        with_env_vars(&[("XDG_CONFIG_HOME", &dir_str), ("HOME", &dir_str)], || {
            assert!(find_config_file().is_none());
        });
    }

    /// When `$XDG_CONFIG_HOME/harness/config.toml` exists, it is returned.
    #[test]
    fn find_config_file_finds_xdg_config_home() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("harness").join("config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, "").unwrap();
        let dir_str = dir.path().to_str().unwrap().to_owned();
        with_env_vars(&[("XDG_CONFIG_HOME", &dir_str), ("HOME", &dir_str)], || {
            let found = find_config_file();
            assert_eq!(found.as_deref(), Some(config_path.as_path()));
        });
    }

    /// XDG path is preferred over the `~/.config` fallback.
    #[test]
    fn find_config_file_prefers_xdg_over_home() {
        let xdg_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();

        let xdg_config = xdg_dir.path().join("harness").join("config.toml");
        fs::create_dir_all(xdg_config.parent().unwrap()).unwrap();
        fs::write(&xdg_config, "# xdg").unwrap();

        let home_config = home_dir
            .path()
            .join(".config")
            .join("harness")
            .join("config.toml");
        fs::create_dir_all(home_config.parent().unwrap()).unwrap();
        fs::write(&home_config, "# home").unwrap();

        let xdg_str = xdg_dir.path().to_str().unwrap().to_owned();
        let home_str = home_dir.path().to_str().unwrap().to_owned();
        with_env_vars(
            &[("XDG_CONFIG_HOME", &xdg_str), ("HOME", &home_str)],
            || {
                let found = find_config_file();
                assert_eq!(found.as_deref(), Some(xdg_config.as_path()));
            },
        );
    }

    /// Relative XDG_CONFIG_HOME values must be rejected (XDG spec requires absolute paths).
    #[test]
    fn find_config_file_rejects_relative_xdg_config_home() {
        let dir = tempfile::tempdir().unwrap();
        let home_str = dir.path().to_str().unwrap().to_owned();
        // XDG_CONFIG_HOME is relative ("relative/path") — must be ignored and fall
        // through to HOME-based ~/.config which does not exist in this temp dir.
        with_env_vars(
            &[("XDG_CONFIG_HOME", "relative/path"), ("HOME", &home_str)],
            || {
                assert!(find_config_file().is_none());
            },
        );
    }

    /// A directory at the candidate path must be skipped (not treated as a file).
    #[test]
    fn find_config_file_skips_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("harness").join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        let dir_str = dir.path().to_str().unwrap().to_owned();
        with_env_vars(&[("XDG_CONFIG_HOME", &dir_str), ("HOME", &dir_str)], || {
            assert!(find_config_file().is_none());
        });
    }
}
