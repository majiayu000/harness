use serde::{Deserialize, Serialize};

/// NAP-Lite observation compression (GH1574). Inert unless `enabled` is
/// true AND `model` is non-empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Model id for the small compressor model. Empty means inert.
    #[serde(default)]
    pub model: String,
    /// Optional OpenAI-compatible endpoint override.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_compression_sample_rate")]
    pub sample_rate: f64,
    #[serde(default = "default_compression_min_size_bytes")]
    pub min_size_bytes: usize,
}

fn default_compression_sample_rate() -> f64 {
    0.10
}

fn default_compression_min_size_bytes() -> usize {
    2_048
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            endpoint: String::new(),
            sample_rate: default_compression_sample_rate(),
            min_size_bytes: default_compression_min_size_bytes(),
        }
    }
}

impl CompressionConfig {
    /// Compression is active only with the flag on and a model configured.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.model.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_inert() {
        let config = CompressionConfig::default();
        assert!(!config.enabled);
        assert!(!config.is_active());
        assert_eq!(config.sample_rate, 0.10);
        assert_eq!(config.min_size_bytes, 2_048);
    }

    #[test]
    fn enabled_without_model_stays_inert() {
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(!config.is_active());
    }

    #[test]
    fn enabled_with_model_is_active_and_toml_parses() {
        let config: CompressionConfig = toml::from_str(
            r#"
enabled = true
model = "claude-haiku-4-5-20251001"
"#,
        )
        .expect("toml parses");
        assert!(config.is_active());
        assert_eq!(config.sample_rate, 0.10);
    }
}
