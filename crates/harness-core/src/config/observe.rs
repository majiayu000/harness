use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveConfig {
    pub session_renewal_secs: u64,
    #[serde(default = "default_log_retention_max_files")]
    pub log_retention_max_files: usize,
    pub log_retention_days: u32,
}

pub fn default_log_retention_max_files() -> usize {
    30
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            session_renewal_secs: 1800,
            log_retention_max_files: default_log_retention_max_files(),
            log_retention_days: 90,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_log_retention_max_files_when_missing() {
        let toml_str = r#"
            session_renewal_secs = 1800
            log_retention_days = 90
        "#;

        let config: ObserveConfig = toml::from_str(toml_str).expect("parse observe config");
        assert_eq!(config.log_retention_max_files, 30);
        assert_eq!(config.log_retention_days, 90);
    }

    #[test]
    fn accepts_zero_log_retention_max_files() {
        let toml_str = r#"
            session_renewal_secs = 1800
            log_retention_max_files = 0
            log_retention_days = 90
        "#;

        let config: ObserveConfig = toml::from_str(toml_str).expect("parse observe config");
        assert_eq!(config.log_retention_max_files, 0);
    }
}
