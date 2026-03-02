use std::path::Path;

/// Detect project languages by checking for common config files.
pub fn detect_languages(project_root: &Path) -> Vec<harness_core::Language> {
    let mut langs = Vec::new();

    if project_root.join("Cargo.toml").exists() {
        langs.push(harness_core::Language::Rust);
    }
    if project_root.join("package.json").exists()
        || project_root.join("tsconfig.json").exists()
    {
        langs.push(harness_core::Language::TypeScript);
    }
    if project_root.join("go.mod").exists() {
        langs.push(harness_core::Language::Go);
    }
    if project_root.join("pyproject.toml").exists()
        || project_root.join("setup.py").exists()
        || project_root.join("requirements.txt").exists()
    {
        langs.push(harness_core::Language::Python);
    }

    langs
}
