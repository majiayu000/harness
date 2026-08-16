use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

const GH1454_CI_CONTRACT_V1_SOURCE: &str =
    include_str!("../../../../../evals/verifiers/gh1454_ci_contract_v1.json");
pub const GH1454_CI_CONTRACT_V1_SHA256: &str =
    "5e72f1bd2d1ce30f10510e17890b3c77e863da3ab44be63aaa3d16c09d46a0f1";
pub const TRUSTED_EVAL_VERIFIER_V1_CAPABILITY: &str = "trusted_eval_verifier_v1";
const GH1454_CASE_ID: &str = "gh1454-scoped-ci-jobs";
const GH1454_REPO: &str = "majiayu000/harness";
const GH1454_ISSUE: u64 = 1454;
const GH1454_BASE_COMMIT: &str = "9c0099ad458e82fd377fd20a8e288a46722762ef";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalTrustedVerifier {
    Gh1454CiContractV1,
}

impl EvalTrustedVerifier {
    pub fn id(self) -> &'static str {
        match self {
            Self::Gh1454CiContractV1 => "gh1454_ci_contract_v1",
        }
    }

    pub fn sha256(self) -> &'static str {
        match self {
            Self::Gh1454CiContractV1 => GH1454_CI_CONTRACT_V1_SHA256,
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::Gh1454CiContractV1 => GH1454_CI_CONTRACT_V1_SOURCE,
        }
    }

    pub fn validation_argv(self) -> Vec<String> {
        vec![
            "harness".to_string(),
            "eval".to_string(),
            "verify-trusted".to_string(),
            self.id().to_string(),
            "--workspace".to_string(),
            ".".to_string(),
            "--verifier-sha256".to_string(),
            self.sha256().to_string(),
        ]
    }
}

pub(super) fn is_trusted_eval_verifier_argv(argv: &[String]) -> bool {
    argv.first().map(String::as_str) == Some("harness")
        && argv.get(1).map(String::as_str) == Some("eval")
        && argv.get(2).map(String::as_str) == Some("verify-trusted")
}

impl FromStr for EvalTrustedVerifier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "gh1454_ci_contract_v1" => Ok(Self::Gh1454CiContractV1),
            _ => Err(format!("unknown trusted eval verifier: {value}")),
        }
    }
}

pub(super) fn trusted_verifier_for_case(
    case_id: &str,
    repo: &str,
    issue: u64,
    base_commit: &str,
) -> Option<EvalTrustedVerifier> {
    if case_id == GH1454_CASE_ID
        && repo == GH1454_REPO
        && issue == GH1454_ISSUE
        && base_commit == GH1454_BASE_COMMIT
    {
        Some(EvalTrustedVerifier::Gh1454CiContractV1)
    } else {
        None
    }
}

pub fn execute_trusted_eval_verifier(
    verifier: EvalTrustedVerifier,
    workspace: &Path,
    expected_sha256: &str,
) -> anyhow::Result<String> {
    if expected_sha256 != verifier.sha256() {
        anyhow::bail!(
            "trusted verifier digest mismatch for {}: expected {}, received {}",
            verifier.id(),
            verifier.sha256(),
            expected_sha256
        );
    }
    let embedded_sha256 = format!("{:x}", Sha256::digest(verifier.source().as_bytes()));
    if embedded_sha256 != verifier.sha256() {
        anyhow::bail!(
            "embedded trusted verifier {} does not match its registered digest",
            verifier.id()
        );
    }
    let contract: TrustedVerifierContract =
        serde_json::from_str(verifier.source()).map_err(|error| {
            anyhow::anyhow!("embedded trusted verifier contract is invalid: {error}")
        })?;
    if contract.verifier_id != verifier.id() {
        anyhow::bail!("embedded trusted verifier contract has the wrong verifier_id");
    }
    let workspace = workspace.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "failed to resolve candidate workspace {}: {error}",
            workspace.display()
        )
    })?;
    let errors = verify_contract(&workspace, &contract);
    let output = TrustedVerifierOutput {
        verifier_id: verifier.id(),
        verifier_sha256: verifier.sha256(),
        passed: errors.is_empty(),
        errors,
    };
    let payload = serde_json::to_string(&output)?;
    if !output.passed {
        anyhow::bail!(
            "trusted verifier {} rejected candidate workspace {}: {}",
            verifier.id(),
            workspace.display(),
            payload
        );
    }
    Ok(payload)
}

#[derive(Serialize)]
struct TrustedVerifierOutput<'a> {
    verifier_id: &'a str,
    verifier_sha256: &'a str,
    passed: bool,
    errors: Vec<String>,
}

#[derive(Deserialize)]
struct TrustedVerifierContract {
    verifier_id: String,
    max_input_bytes: u64,
    files: Vec<TrustedVerifierFileContract>,
}

#[derive(Deserialize)]
struct TrustedVerifierFileContract {
    path: String,
    forbidden_trimmed_lines: Vec<String>,
    exact_counts: BTreeMap<String, usize>,
    required_fragments: Vec<String>,
}

fn verify_contract(workspace: &Path, contract: &TrustedVerifierContract) -> Vec<String> {
    let mut errors = Vec::new();
    for file in &contract.files {
        let source = match read_workspace_file(workspace, &file.path, contract.max_input_bytes) {
            Ok(source) => source,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        for forbidden in &file.forbidden_trimmed_lines {
            if source.lines().any(|line| line.trim() == forbidden) {
                errors.push(format!("forbidden trimmed line is present: {forbidden}"));
            }
        }
        for (fragment, expected) in &file.exact_counts {
            let observed = source.matches(fragment).count();
            if observed != *expected {
                errors.push(format!(
                    "contract fragment {fragment:?} occurs {observed} times; expected {expected}"
                ));
            }
        }
        require_fragments(&source, &file.required_fragments, &mut errors);
    }
    errors
}

fn read_workspace_file(
    workspace: &Path,
    relative: &str,
    max_input_bytes: u64,
) -> Result<String, String> {
    let path = workspace.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("failed to inspect {relative}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{relative} must be a regular file, not a symlink"));
    }
    if metadata.len() > max_input_bytes {
        return Err(format!(
            "{relative} exceeds the {max_input_bytes}-byte verifier limit"
        ));
    }
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {relative}: {error}"))?;
    if !resolved.starts_with(workspace) {
        return Err(format!(
            "{relative} resolves outside the candidate workspace"
        ));
    }
    fs::read_to_string(&path).map_err(|error| format!("failed to read {relative}: {error}"))
}

fn require_fragments(source: &str, required: &[String], errors: &mut Vec<String>) {
    for fragment in required {
        if !source.contains(fragment) {
            errors.push(format!("missing required contract fragment: {fragment}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_verifier_digest_matches_embedded_asset() {
        let verifier = EvalTrustedVerifier::Gh1454CiContractV1;
        let digest = format!("{:x}", Sha256::digest(verifier.source().as_bytes()));

        assert_eq!(digest, verifier.sha256());
        assert!(verifier.validation_argv().contains(&digest));
    }

    #[test]
    fn digest_mismatch_fails_before_workspace_access() {
        let error = execute_trusted_eval_verifier(
            EvalTrustedVerifier::Gh1454CiContractV1,
            Path::new("missing-workspace"),
            &"0".repeat(64),
        )
        .expect_err("digest mismatch must fail first");

        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn verifier_registry_is_bound_to_the_historical_case_id() {
        assert_eq!(
            trusted_verifier_for_case(
                GH1454_CASE_ID,
                GH1454_REPO,
                GH1454_ISSUE,
                GH1454_BASE_COMMIT,
            ),
            Some(EvalTrustedVerifier::Gh1454CiContractV1)
        );
        assert_eq!(
            trusted_verifier_for_case(GH1454_CASE_ID, GH1454_REPO, GH1454_ISSUE, "9c0099ad",),
            None
        );
    }

    #[test]
    fn native_verifier_emits_bound_success_schema() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join(".github/workflows"))?;
        fs::create_dir_all(directory.path().join(".githooks"))?;
        fs::write(
            directory.path().join(".github/workflows/ci.yml"),
            r#"
web-build:
  run: bun run build
workspace: ${{ steps.filter.outputs.workspace }}
other_crates: ${{ steps.filter.outputs.other_crates }}
actions/upload-artifact@v4
actions/download-artifact@v4
HARNESS_SKIP_WEB_BUILD: "1"
name: Compute test scope
steps.scope.outputs.packages
needs.web-build.result
"#,
        )?;
        fs::write(
            directory.path().join(".githooks/pre-commit"),
            r#"
derive_scope()
git diff --cached --name-only
Cargo.toml|Cargo.lock)
crates/*/*)
cargo clippy $scope --all-targets -- -D warnings
cargo test $scope --lib
"#,
        )?;
        let verifier = EvalTrustedVerifier::Gh1454CiContractV1;

        let output = execute_trusted_eval_verifier(verifier, directory.path(), verifier.sha256())?;
        let value: serde_json::Value = serde_json::from_str(&output)?;

        assert_eq!(value["verifier_id"], verifier.id());
        assert_eq!(value["verifier_sha256"], verifier.sha256());
        assert_eq!(value["passed"], true);
        assert_eq!(value["errors"], serde_json::json!([]));
        Ok(())
    }
}
