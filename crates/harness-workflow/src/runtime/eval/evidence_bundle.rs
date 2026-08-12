use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};
use thiserror::Error;

pub const EVIDENCE_BUNDLE_SCHEMA_VERSION: &str = "change-control-evidence-bundle/v0.1";
pub const EVIDENCE_BUNDLE_MANIFEST_SCHEMA_VERSION: &str =
    "change-control-evidence-bundle-manifest/v0.1";
pub const EVIDENCE_BUNDLE_ARTIFACT_SCHEMA_VERSION: &str = "change-control-evidence-artifact/v0.1";
pub const REDACTED_VALUE: &str = "[REDACTED]";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACT_DIR: &str = "artifacts";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBundleArtifactKind {
    Stack,
    Diff,
    Suite,
    Results,
    Comparison,
    Policy,
    Summary,
}

impl EvidenceBundleArtifactKind {
    pub const ALL: &'static [Self] = &[
        Self::Stack,
        Self::Diff,
        Self::Suite,
        Self::Results,
        Self::Comparison,
        Self::Policy,
        Self::Summary,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::Diff => "diff",
            Self::Suite => "suite",
            Self::Results => "results",
            Self::Comparison => "comparison",
            Self::Policy => "policy",
            Self::Summary => "summary",
        }
    }

    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Stack => "stack.json",
            Self::Diff => "diff.json",
            Self::Suite => "suite.json",
            Self::Results => "results.json",
            Self::Comparison => "comparison.json",
            Self::Policy => "policy.json",
            Self::Summary => "summary.json",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceBundleArtifactInput {
    kind: EvidenceBundleArtifactKind,
    value: Value,
}

impl EvidenceBundleArtifactInput {
    pub fn new(kind: EvidenceBundleArtifactKind, value: Value) -> Self {
        Self { kind, value }
    }

    pub const fn kind(&self) -> EvidenceBundleArtifactKind {
        self.kind
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleManifest {
    pub schema_version: String,
    pub bundle_schema_version: String,
    pub bundle_id: String,
    pub required_artifacts: Vec<EvidenceBundleArtifactKind>,
    pub files: Vec<EvidenceBundleFileManifest>,
    pub artifact_count: u64,
    pub redaction_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleFileManifest {
    pub path: String,
    pub artifact_type: EvidenceBundleArtifactKind,
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_schema_version: Option<String>,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub redactions: Vec<EvidenceBundleRedaction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleRedaction {
    pub json_pointer: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleVerification {
    pub bundle_id: String,
    pub manifest_path: String,
    pub files_verified: u64,
}

#[derive(Debug, Error)]
pub enum EvidenceBundleError {
    #[error("evidence bundle id must not be empty")]
    EmptyBundleId,
    #[error("evidence bundle is missing required artifact `{0}`")]
    MissingRequiredArtifact(&'static str),
    #[error("evidence bundle contains duplicate artifact `{0}`")]
    DuplicateArtifact(&'static str),
    #[error("bundle path `{0}` escapes the bundle directory")]
    InvalidBundlePath(String),
    #[error("bundle file `{path}` digest mismatch: expected {expected}, actual {actual}")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("bundle file `{path}` byte length mismatch: expected {expected}, actual {actual}")]
    ByteLengthMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("failed to read `{path}`")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write `{path}`")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory `{path}`")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in `{path}`")]
    JsonFile {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct EvidenceBundleArtifactDocument {
    schema_version: String,
    artifact_type: EvidenceBundleArtifactKind,
    content_schema_version: Option<String>,
    redactions: Vec<EvidenceBundleRedaction>,
    content: Value,
}

pub fn read_evidence_bundle_artifact(
    kind: EvidenceBundleArtifactKind,
    path: &Path,
) -> Result<EvidenceBundleArtifactInput, EvidenceBundleError> {
    let content = fs::read_to_string(path).map_err(|source| EvidenceBundleError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let value = serde_json::from_str(&content).map_err(|source| EvidenceBundleError::JsonFile {
        path: path.display().to_string(),
        source,
    })?;
    Ok(EvidenceBundleArtifactInput::new(kind, value))
}

pub fn write_evidence_bundle(
    output_dir: &Path,
    bundle_id: impl Into<String>,
    artifacts: Vec<EvidenceBundleArtifactInput>,
) -> Result<EvidenceBundleManifest, EvidenceBundleError> {
    let bundle_id = bundle_id.into();
    if bundle_id.trim().is_empty() {
        return Err(EvidenceBundleError::EmptyBundleId);
    }

    let mut artifacts_by_kind = index_artifacts(artifacts)?;
    create_dir(output_dir)?;
    create_dir(&output_dir.join(ARTIFACT_DIR))?;

    let mut files = Vec::with_capacity(EvidenceBundleArtifactKind::ALL.len());
    for kind in EvidenceBundleArtifactKind::ALL {
        let input = artifacts_by_kind
            .remove(kind)
            .ok_or(EvidenceBundleError::MissingRequiredArtifact(kind.as_str()))?;
        let (content, redactions) = redacted_value(input.value());
        let document = EvidenceBundleArtifactDocument {
            schema_version: EVIDENCE_BUNDLE_ARTIFACT_SCHEMA_VERSION.to_string(),
            artifact_type: *kind,
            content_schema_version: schema_version(&content),
            redactions: redactions.clone(),
            content,
        };
        let bytes = deterministic_json_bytes(&document)?;
        let relative_path = format!("{ARTIFACT_DIR}/{}", kind.file_name());
        write_file(&output_dir.join(&relative_path), &bytes)?;
        files.push(EvidenceBundleFileManifest {
            path: relative_path,
            artifact_type: *kind,
            schema_version: EVIDENCE_BUNDLE_ARTIFACT_SCHEMA_VERSION.to_string(),
            content_schema_version: document.content_schema_version,
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
            redactions,
        });
    }

    let manifest = EvidenceBundleManifest {
        schema_version: EVIDENCE_BUNDLE_MANIFEST_SCHEMA_VERSION.to_string(),
        bundle_schema_version: EVIDENCE_BUNDLE_SCHEMA_VERSION.to_string(),
        bundle_id,
        required_artifacts: EvidenceBundleArtifactKind::ALL.to_vec(),
        artifact_count: files.len() as u64,
        redaction_count: files.iter().map(|file| file.redactions.len() as u64).sum(),
        files,
    };
    let manifest_bytes = deterministic_json_bytes(&manifest)?;
    write_file(&output_dir.join(MANIFEST_FILE), &manifest_bytes)?;
    Ok(manifest)
}

pub fn verify_evidence_bundle(
    output_dir: &Path,
) -> Result<EvidenceBundleVerification, EvidenceBundleError> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    let manifest_bytes =
        fs::read(&manifest_path).map_err(|source| EvidenceBundleError::ReadFile {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let manifest: EvidenceBundleManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| {
            EvidenceBundleError::JsonFile {
                path: manifest_path.display().to_string(),
                source,
            }
        })?;
    validate_manifest_completeness(&manifest)?;

    for file in &manifest.files {
        validate_relative_bundle_path(&file.path)?;
        let path = output_dir.join(&file.path);
        let bytes = fs::read(&path).map_err(|source| EvidenceBundleError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        let actual_len = bytes.len() as u64;
        if actual_len != file.bytes {
            return Err(EvidenceBundleError::ByteLengthMismatch {
                path: file.path.clone(),
                expected: file.bytes,
                actual: actual_len,
            });
        }
        let actual_digest = sha256_hex(&bytes);
        if actual_digest != file.sha256 {
            return Err(EvidenceBundleError::DigestMismatch {
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual: actual_digest,
            });
        }
    }

    Ok(EvidenceBundleVerification {
        bundle_id: manifest.bundle_id,
        manifest_path: MANIFEST_FILE.to_string(),
        files_verified: manifest.files.len() as u64,
    })
}

pub fn evidence_bundle_manifest_json(
    manifest: &EvidenceBundleManifest,
) -> Result<String, EvidenceBundleError> {
    let bytes = deterministic_json_bytes(manifest)?;
    String::from_utf8(bytes).map_err(|error| {
        serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)).into()
    })
}

fn index_artifacts(
    artifacts: Vec<EvidenceBundleArtifactInput>,
) -> Result<BTreeMap<EvidenceBundleArtifactKind, EvidenceBundleArtifactInput>, EvidenceBundleError>
{
    let mut by_kind = BTreeMap::new();
    for artifact in artifacts {
        let kind = artifact.kind();
        if by_kind.insert(kind, artifact).is_some() {
            return Err(EvidenceBundleError::DuplicateArtifact(kind.as_str()));
        }
    }
    for kind in EvidenceBundleArtifactKind::ALL {
        if !by_kind.contains_key(kind) {
            return Err(EvidenceBundleError::MissingRequiredArtifact(kind.as_str()));
        }
    }
    Ok(by_kind)
}

fn validate_manifest_completeness(
    manifest: &EvidenceBundleManifest,
) -> Result<(), EvidenceBundleError> {
    let mut seen = BTreeSet::new();
    for file in &manifest.files {
        if !seen.insert(file.artifact_type) {
            return Err(EvidenceBundleError::DuplicateArtifact(
                file.artifact_type.as_str(),
            ));
        }
    }
    for kind in EvidenceBundleArtifactKind::ALL {
        if !seen.contains(kind) {
            return Err(EvidenceBundleError::MissingRequiredArtifact(kind.as_str()));
        }
    }
    Ok(())
}

fn deterministic_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, EvidenceBundleError> {
    let value = serde_json::to_value(value)?;
    let mut output = String::new();
    write_deterministic_json_value(&value, &mut output)?;
    output.push('\n');
    Ok(output.into_bytes())
}

fn write_deterministic_json_value(
    value: &Value,
    output: &mut String,
) -> Result<(), EvidenceBundleError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value)?),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_deterministic_json_value(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key)?);
                output.push(':');
                let child = values
                    .get(key)
                    .expect("object key from keys iterator must exist");
                write_deterministic_json_value(child, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn redacted_value(value: &Value) -> (Value, Vec<EvidenceBundleRedaction>) {
    redact_at(value, "")
}

fn redact_at(value: &Value, path: &str) -> (Value, Vec<EvidenceBundleRedaction>) {
    match value {
        Value::Object(values) => {
            let mut redactions = Vec::new();
            let mut redacted = serde_json::Map::with_capacity(values.len());
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                let child = values
                    .get(key)
                    .expect("object key from keys iterator must exist");
                let child_path = json_pointer_child(path, key);
                if let Some(reason) = redaction_reason(key) {
                    redactions.push(EvidenceBundleRedaction {
                        json_pointer: child_path,
                        reason: reason.to_string(),
                    });
                    redacted.insert(key.clone(), Value::String(REDACTED_VALUE.to_string()));
                } else {
                    let (value, mut child_redactions) = redact_at(child, &child_path);
                    redactions.append(&mut child_redactions);
                    redacted.insert(key.clone(), value);
                }
            }
            (Value::Object(redacted), redactions)
        }
        Value::Array(values) => {
            let mut redactions = Vec::new();
            let values = values
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    let child_path = json_pointer_child(path, &index.to_string());
                    let (value, mut child_redactions) = redact_at(child, &child_path);
                    redactions.append(&mut child_redactions);
                    value
                })
                .collect();
            (Value::Array(values), redactions)
        }
        _ => (value.clone(), Vec::new()),
    }
}

fn redaction_reason(key: &str) -> Option<&'static str> {
    let normalized = key
        .chars()
        .map(|ch| match ch {
            '-' | ' ' => '_',
            _ => ch.to_ascii_lowercase(),
        })
        .collect::<String>();
    if normalized.contains("private_memory") || normalized.contains("memory_secret") {
        return Some("private_memory");
    }
    if normalized == "token"
        || normalized.ends_with("_token")
        || normalized == "password"
        || normalized.ends_with("_password")
        || normalized.contains("secret")
        || normalized == "api_key"
        || normalized.ends_with("_api_key")
        || normalized.contains("apikey")
        || normalized.contains("credential")
        || normalized.contains("private_key")
    {
        return Some("secret");
    }
    None
}

fn json_pointer_child(parent: &str, child: &str) -> String {
    let escaped = child.replace('~', "~0").replace('/', "~1");
    if parent.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{parent}/{escaped}")
    }
}

fn schema_version(value: &Value) -> Option<String> {
    value
        .get("schema_version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn create_dir(path: &Path) -> Result<(), EvidenceBundleError> {
    fs::create_dir_all(path).map_err(|source| EvidenceBundleError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), EvidenceBundleError> {
    fs::write(path, bytes).map_err(|source| EvidenceBundleError::WriteFile {
        path: path.display().to_string(),
        source,
    })
}

fn validate_relative_bundle_path(path: &str) -> Result<(), EvidenceBundleError> {
    let path_value = Path::new(path);
    let valid = !path_value.is_absolute()
        && path_value
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(EvidenceBundleError::InvalidBundlePath(path.to_string()))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn evidence_bundle_requires_every_artifact() {
        let error = write_evidence_bundle(
            tempdir().expect("tempdir should be created").path(),
            "bundle-1",
            vec![artifact(EvidenceBundleArtifactKind::Stack, json!({}))],
        )
        .expect_err("missing artifacts should be rejected");

        assert!(error
            .to_string()
            .contains("missing required artifact `diff`"));
    }

    #[test]
    fn evidence_bundle_redacts_sensitive_values_and_detects_tampering() {
        let tempdir = tempdir().expect("tempdir should be created");
        let manifest = write_evidence_bundle(
            tempdir.path(),
            "bundle-1",
            complete_artifacts(
                json!({
                    "schema_version": "stack/v1",
                    "components": [
                        {
                            "id": "runtime:memory:private",
                            "private_memory": "keep me out"
                        }
                    ]
                }),
                json!({
                    "schema_version": "policy/v1",
                    "github_token": "ghp_secret",
                    "decision": "review"
                }),
            ),
        )
        .expect("bundle should be written");

        assert_eq!(manifest.artifact_count, 7);
        assert_eq!(manifest.redaction_count, 2);
        let stack =
            fs::read_to_string(tempdir.path().join("artifacts/stack.json")).expect("stack exists");
        assert!(!stack.contains("keep me out"));
        assert!(stack.contains(REDACTED_VALUE));
        let policy = fs::read_to_string(tempdir.path().join("artifacts/policy.json"))
            .expect("policy exists");
        assert!(!policy.contains("ghp_secret"));
        assert!(policy.contains("\"/github_token\""));
        verify_evidence_bundle(tempdir.path()).expect("fresh bundle should verify");

        fs::write(
            tempdir.path().join("artifacts/policy.json"),
            policy.replace("review", "denied"),
        )
        .expect("tamper write should succeed");
        let error = verify_evidence_bundle(tempdir.path()).expect_err("tampering must be rejected");
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn evidence_bundle_output_is_reproducible_for_equivalent_json() {
        let first = tempdir().expect("first tempdir should be created");
        let second = tempdir().expect("second tempdir should be created");
        write_evidence_bundle(
            first.path(),
            "bundle-1",
            complete_artifacts(
                json!({
                    "schema_version": "stack/v1",
                    "nested": {
                        "b": 2,
                        "a": 1
                    }
                }),
                json!({
                    "schema_version": "policy/v1",
                    "api_key": "secret-a",
                    "password": "secret-b",
                    "rules": ["review"]
                }),
            ),
        )
        .expect("first bundle should be written");
        write_evidence_bundle(
            second.path(),
            "bundle-1",
            complete_artifacts(
                json!({
                    "nested": {
                        "a": 1,
                        "b": 2
                    },
                    "schema_version": "stack/v1"
                }),
                json!({
                    "rules": ["review"],
                    "password": "secret-b",
                    "api_key": "secret-a",
                    "schema_version": "policy/v1"
                }),
            ),
        )
        .expect("second bundle should be written");

        for path in [
            "manifest.json",
            "artifacts/stack.json",
            "artifacts/diff.json",
            "artifacts/suite.json",
            "artifacts/results.json",
            "artifacts/comparison.json",
            "artifacts/policy.json",
            "artifacts/summary.json",
        ] {
            let first_bytes = fs::read(first.path().join(path)).expect("first file should exist");
            let second_bytes =
                fs::read(second.path().join(path)).expect("second file should exist");
            assert_eq!(first_bytes, second_bytes, "{path} should be reproducible");
        }
    }

    fn complete_artifacts(stack: Value, policy: Value) -> Vec<EvidenceBundleArtifactInput> {
        vec![
            artifact(EvidenceBundleArtifactKind::Stack, stack),
            artifact(
                EvidenceBundleArtifactKind::Diff,
                json!({"schema_version": "diff/v1", "changes": []}),
            ),
            artifact(
                EvidenceBundleArtifactKind::Suite,
                json!({"schema_version": "suite/v1", "suite": "harness-core"}),
            ),
            artifact(
                EvidenceBundleArtifactKind::Results,
                json!({"schema_version": "results/v1", "cases": []}),
            ),
            artifact(
                EvidenceBundleArtifactKind::Comparison,
                json!({"schema_version": "comparison/v1", "regression_count": 0}),
            ),
            artifact(EvidenceBundleArtifactKind::Policy, policy),
            artifact(
                EvidenceBundleArtifactKind::Summary,
                json!({"schema_version": "summary/v1", "decision": "review"}),
            ),
        ]
    }

    fn artifact(kind: EvidenceBundleArtifactKind, value: Value) -> EvidenceBundleArtifactInput {
        EvidenceBundleArtifactInput::new(kind, value)
    }
}
