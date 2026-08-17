use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    Flag(PathBuf, bool),
    Discovered(PathBuf, bool),
    BuiltInDefaults,
}

impl ConfigSource {
    /// Path of the loaded config file, if any (built-in defaults have none).
    pub(crate) fn config_path(&self) -> Option<&Path> {
        match self {
            ConfigSource::Flag(path, _) | ConfigSource::Discovered(path, _) => Some(path.as_path()),
            ConfigSource::BuiltInDefaults => None,
        }
    }

    pub(crate) fn capability_profile_defaulted(&self) -> bool {
        match self {
            ConfigSource::Flag(_, defaulted) | ConfigSource::Discovered(_, defaulted) => *defaulted,
            ConfigSource::BuiltInDefaults => true,
        }
    }
}

pub(crate) fn load_config(
    config_path: Option<&Path>,
) -> anyhow::Result<(harness_core::config::HarnessConfig, ConfigSource)> {
    if let Some(config_path) = config_path {
        let content = fs::read_to_string(config_path)?;
        let mut config: harness_core::config::HarnessConfig = toml::from_str(&content)?;
        let capability_profile_defaulted = capability_profile_defaulted(&content)?;
        if let Some(dir) = config_path.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok((
            config,
            ConfigSource::Flag(config_path.to_path_buf(), capability_profile_defaulted),
        ));
    }

    if let Some(discovered) = harness_core::config::dirs::find_config_file() {
        let content = fs::read_to_string(&discovered)?;
        let mut config: harness_core::config::HarnessConfig = toml::from_str(&content)?;
        let capability_profile_defaulted = capability_profile_defaulted(&content)?;
        if let Some(dir) = discovered.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok((
            config,
            ConfigSource::Discovered(discovered, capability_profile_defaulted),
        ));
    }

    Ok((
        harness_core::config::HarnessConfig::default(),
        ConfigSource::BuiltInDefaults,
    ))
}

pub(crate) fn capability_profile_defaulted(content: &str) -> anyhow::Result<bool> {
    let document: toml::Value = toml::from_str(content)?;
    Ok(document
        .get("agents")
        .and_then(|agents| agents.get("capability_profile"))
        .is_none())
}

pub(crate) fn log_config_source(source: &ConfigSource) {
    match source {
        ConfigSource::Flag(path, _) => {
            tracing::info!("config loaded from --config flag: {}", path.display());
        }
        ConfigSource::Discovered(path, _) => {
            tracing::info!("config loaded from {}", path.display());
        }
        ConfigSource::BuiltInDefaults => {
            tracing::warn!("no config file found, using built-in defaults");
        }
    }
}

pub(crate) fn install_workflow_base(source: &ConfigSource) -> anyhow::Result<()> {
    // Register the central base WORKFLOW.md (sibling of the loaded config file,
    // e.g. config/WORKFLOW.md) as the single source of default workflow policy.
    // Per-repo WORKFLOW.md files deep-merge on top of it field-by-field. The
    // path is resolved to an absolute location so it does not depend on the
    // server process's working directory.
    if let Some(config_dir) = source.config_path().and_then(Path::parent) {
        let base = config_dir.join("WORKFLOW.md");
        if base.try_exists()? {
            let base = std::fs::canonicalize(&base)?;
            tracing::info!("central workflow base config: {}", base.display());
            harness_core::config::workflow::set_workflow_base_path(base);
        } else {
            tracing::info!(
                "no central workflow base config at {} (per-repo WORKFLOW.md only)",
                base.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::capability_profile_defaulted;

    #[test]
    fn capability_profile_migration_warning_only_applies_when_field_is_absent() -> anyhow::Result<()>
    {
        assert!(capability_profile_defaulted(
            "[agents]\ndefault_agent = \"auto\"\n"
        )?);
        assert!(!capability_profile_defaulted(
            "[agents]\ncapability_profile = \"full\"\n"
        )?);
        Ok(())
    }
}
