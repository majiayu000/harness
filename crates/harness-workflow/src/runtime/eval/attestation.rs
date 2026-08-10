use harness_core::stack::Sha256Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{error::Error, fmt};

pub const EVAL_RUN_ATTESTATION_SCHEMA_VERSION: &str = "eval-run-attestation/v0.1";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalAttestationDecision {
    Approved,
    Rejected,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalAttestationTrust {
    #[default]
    Unsigned,
    Unverified,
    Verified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalAttestationSummary {
    pub trust: EvalAttestationTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<EvalAttestationDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_error: Option<String>,
}

impl Default for EvalAttestationSummary {
    fn default() -> Self {
        Self::unsigned()
    }
}

impl EvalAttestationSummary {
    pub fn unsigned() -> Self {
        Self {
            trust: EvalAttestationTrust::Unsigned,
            provider: None,
            runner_identity: None,
            commit: None,
            stack_id: None,
            suite_digest: None,
            manifest_digest: None,
            decision: None,
            verification_error: None,
        }
    }

    pub fn verified(attestation: &EvalRunAttestation) -> Self {
        Self::from_attestation(attestation, EvalAttestationTrust::Verified, None)
    }

    pub fn unverified(
        attestation: &EvalRunAttestation,
        error: &EvalAttestationVerificationError,
    ) -> Self {
        Self::from_attestation(
            attestation,
            EvalAttestationTrust::Unverified,
            Some(error.to_string()),
        )
    }

    pub fn is_approved(&self) -> bool {
        self.trust == EvalAttestationTrust::Verified
            && self.decision == Some(EvalAttestationDecision::Approved)
    }

    fn from_attestation(
        attestation: &EvalRunAttestation,
        trust: EvalAttestationTrust,
        verification_error: Option<String>,
    ) -> Self {
        Self {
            trust,
            provider: Some(attestation.provider.clone()),
            runner_identity: Some(attestation.claims.runner_identity.clone()),
            commit: Some(attestation.claims.commit.clone()),
            stack_id: Some(attestation.claims.stack_id.clone()),
            suite_digest: Some(attestation.claims.suite_digest.clone()),
            manifest_digest: Some(attestation.claims.manifest_digest.clone()),
            decision: Some(attestation.claims.decision),
            verification_error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunAttestationClaims {
    pub runner_identity: String,
    pub commit: String,
    pub stack_id: String,
    pub suite_digest: String,
    pub manifest_digest: String,
    pub decision: EvalAttestationDecision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunAttestation {
    pub schema_version: String,
    pub provider: String,
    pub signature: String,
    pub payload_digest: String,
    pub claims: EvalRunAttestationClaims,
}

impl EvalRunAttestation {
    pub fn new(
        provider: impl Into<String>,
        signature: impl Into<String>,
        claims: EvalRunAttestationClaims,
    ) -> Self {
        let payload_digest = eval_run_attestation_payload_digest(&claims);
        Self {
            schema_version: EVAL_RUN_ATTESTATION_SCHEMA_VERSION.to_string(),
            provider: provider.into(),
            signature: signature.into(),
            payload_digest,
            claims,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalRunAttestationExpected {
    pub claims: EvalRunAttestationClaims,
    pub audience: String,
    pub subjects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeylessOidcVerification {
    pub runner_identity: String,
    pub audience: String,
    pub subjects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEvalRunAttestation {
    pub provider: String,
    pub claims: EvalRunAttestationClaims,
    pub audience: String,
    pub subjects: Vec<String>,
}

pub trait KeylessOidcProvider {
    fn provider_id(&self) -> &str;

    fn verify_signature(
        &self,
        attestation: &EvalRunAttestation,
    ) -> Result<KeylessOidcVerification, EvalAttestationVerificationError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalAttestationVerificationError {
    UnsupportedSchemaVersion {
        expected: &'static str,
        actual: String,
    },
    ProviderMismatch {
        expected: String,
        actual: String,
    },
    MissingSignature,
    InvalidPayloadDigest {
        value: String,
    },
    PayloadDigestMismatch {
        expected: String,
        actual: String,
    },
    ClaimMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    MissingAudienceExpectation,
    MissingSubjectExpectation,
    SignatureInvalid {
        reason: String,
    },
    IdentityMismatch {
        expected: String,
        actual: String,
    },
    AudienceMismatch {
        expected: String,
        actual: String,
    },
    SubjectsMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
}

impl fmt::Display for EvalAttestationVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { expected, actual } => write!(
                f,
                "unsupported attestation schema version `{actual}`; expected `{expected}`"
            ),
            Self::ProviderMismatch { expected, actual } => write!(
                f,
                "attestation provider `{actual}` does not match verifier `{expected}`"
            ),
            Self::MissingSignature => f.write_str("attestation signature is missing"),
            Self::InvalidPayloadDigest { value } => {
                write!(f, "attestation payload_digest `{value}` is invalid")
            }
            Self::PayloadDigestMismatch { expected, actual } => write!(
                f,
                "attestation payload_digest `{actual}` does not match claims digest `{expected}`"
            ),
            Self::ClaimMismatch {
                field,
                expected,
                actual,
            } => write!(
                f,
                "attestation claim `{field}` mismatch: expected `{expected}`, got `{actual}`"
            ),
            Self::MissingAudienceExpectation => {
                f.write_str("attestation audience expectation is missing")
            }
            Self::MissingSubjectExpectation => {
                f.write_str("attestation subject expectation is missing")
            }
            Self::SignatureInvalid { reason } => {
                write!(f, "attestation signature is invalid: {reason}")
            }
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "attestation identity mismatch: expected `{expected}`, got `{actual}`"
            ),
            Self::AudienceMismatch { expected, actual } => write!(
                f,
                "attestation audience mismatch: expected `{expected}`, got `{actual}`"
            ),
            Self::SubjectsMismatch { expected, actual } => write!(
                f,
                "attestation subjects mismatch: expected {:?}, got {:?}",
                expected, actual
            ),
        }
    }
}

impl Error for EvalAttestationVerificationError {}

pub fn eval_run_attestation_payload_digest(claims: &EvalRunAttestationClaims) -> String {
    let encoded =
        serde_json::to_vec(claims).expect("EvalRunAttestationClaims serialization is infallible");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

pub fn verify_eval_run_attestation(
    attestation: &EvalRunAttestation,
    expected: &EvalRunAttestationExpected,
    provider: &impl KeylessOidcProvider,
) -> Result<VerifiedEvalRunAttestation, EvalAttestationVerificationError> {
    if attestation.schema_version != EVAL_RUN_ATTESTATION_SCHEMA_VERSION {
        return Err(EvalAttestationVerificationError::UnsupportedSchemaVersion {
            expected: EVAL_RUN_ATTESTATION_SCHEMA_VERSION,
            actual: attestation.schema_version.clone(),
        });
    }
    if provider.provider_id() != attestation.provider {
        return Err(EvalAttestationVerificationError::ProviderMismatch {
            expected: provider.provider_id().to_string(),
            actual: attestation.provider.clone(),
        });
    }
    if attestation.signature.trim().is_empty() {
        return Err(EvalAttestationVerificationError::MissingSignature);
    }
    validate_prefixed_sha256(&attestation.payload_digest)?;
    let expected_payload_digest = eval_run_attestation_payload_digest(&attestation.claims);
    if attestation.payload_digest != expected_payload_digest {
        return Err(EvalAttestationVerificationError::PayloadDigestMismatch {
            expected: expected_payload_digest,
            actual: attestation.payload_digest.clone(),
        });
    }
    if expected.audience.trim().is_empty() {
        return Err(EvalAttestationVerificationError::MissingAudienceExpectation);
    }
    if expected.subjects.is_empty()
        || expected
            .subjects
            .iter()
            .any(|subject| subject.trim().is_empty())
    {
        return Err(EvalAttestationVerificationError::MissingSubjectExpectation);
    }
    compare_claims(&expected.claims, &attestation.claims)?;

    let verification = provider.verify_signature(attestation)?;
    if verification.runner_identity != expected.claims.runner_identity {
        return Err(EvalAttestationVerificationError::IdentityMismatch {
            expected: expected.claims.runner_identity.clone(),
            actual: verification.runner_identity,
        });
    }
    if verification.audience != expected.audience {
        return Err(EvalAttestationVerificationError::AudienceMismatch {
            expected: expected.audience.clone(),
            actual: verification.audience,
        });
    }
    if verification.subjects != expected.subjects {
        return Err(EvalAttestationVerificationError::SubjectsMismatch {
            expected: expected.subjects.clone(),
            actual: verification.subjects,
        });
    }

    Ok(VerifiedEvalRunAttestation {
        provider: attestation.provider.clone(),
        claims: attestation.claims.clone(),
        audience: expected.audience.clone(),
        subjects: expected.subjects.clone(),
    })
}

pub fn classify_eval_run_attestation(
    attestation: Option<&EvalRunAttestation>,
    expected: &EvalRunAttestationExpected,
    provider: &impl KeylessOidcProvider,
) -> EvalAttestationSummary {
    let Some(attestation) = attestation else {
        return EvalAttestationSummary::unsigned();
    };

    match verify_eval_run_attestation(attestation, expected, provider) {
        Ok(_) => EvalAttestationSummary::verified(attestation),
        Err(error) => EvalAttestationSummary::unverified(attestation, &error),
    }
}

fn compare_claims(
    expected: &EvalRunAttestationClaims,
    actual: &EvalRunAttestationClaims,
) -> Result<(), EvalAttestationVerificationError> {
    compare_claim(
        "runner_identity",
        &expected.runner_identity,
        &actual.runner_identity,
    )?;
    compare_commit(&expected.commit, &actual.commit)?;
    compare_claim("stack_id", &expected.stack_id, &actual.stack_id)?;
    compare_digest("suite_digest", &expected.suite_digest, &actual.suite_digest)?;
    compare_digest(
        "manifest_digest",
        &expected.manifest_digest,
        &actual.manifest_digest,
    )?;
    if expected.decision != actual.decision {
        return Err(EvalAttestationVerificationError::ClaimMismatch {
            field: "decision",
            expected: format!("{:?}", expected.decision),
            actual: format!("{:?}", actual.decision),
        });
    }
    Ok(())
}

fn compare_commit(expected: &str, actual: &str) -> Result<(), EvalAttestationVerificationError> {
    if !is_valid_commit(expected) || !is_valid_commit(actual) {
        return Err(EvalAttestationVerificationError::ClaimMismatch {
            field: "commit",
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    compare_claim("commit", expected, actual)
}

fn compare_digest(
    field: &'static str,
    expected: &str,
    actual: &str,
) -> Result<(), EvalAttestationVerificationError> {
    if validate_prefixed_sha256(expected).is_err() || validate_prefixed_sha256(actual).is_err() {
        return Err(EvalAttestationVerificationError::ClaimMismatch {
            field,
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    compare_claim(field, expected, actual)
}

fn compare_claim(
    field: &'static str,
    expected: &str,
    actual: &str,
) -> Result<(), EvalAttestationVerificationError> {
    if expected.trim().is_empty() || actual.trim().is_empty() || expected != actual {
        return Err(EvalAttestationVerificationError::ClaimMismatch {
            field,
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

fn validate_prefixed_sha256(value: &str) -> Result<(), EvalAttestationVerificationError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(EvalAttestationVerificationError::InvalidPayloadDigest {
            value: value.to_string(),
        });
    };
    Sha256Digest::parse(digest).map_err(|_| {
        EvalAttestationVerificationError::InvalidPayloadDigest {
            value: value.to_string(),
        }
    })?;
    Ok(())
}

fn is_valid_commit(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OfflineProvider;
    struct WrongIdentityProvider;

    impl KeylessOidcProvider for OfflineProvider {
        fn provider_id(&self) -> &str {
            "offline-oidc"
        }

        fn verify_signature(
            &self,
            attestation: &EvalRunAttestation,
        ) -> Result<KeylessOidcVerification, EvalAttestationVerificationError> {
            offline_verification(attestation)
        }
    }

    impl KeylessOidcProvider for WrongIdentityProvider {
        fn provider_id(&self) -> &str {
            "offline-oidc"
        }

        fn verify_signature(
            &self,
            attestation: &EvalRunAttestation,
        ) -> Result<KeylessOidcVerification, EvalAttestationVerificationError> {
            let mut verification = offline_verification(attestation)?;
            verification.runner_identity = "repo:other/workflow".to_string();
            Ok(verification)
        }
    }

    fn offline_verification(
        attestation: &EvalRunAttestation,
    ) -> Result<KeylessOidcVerification, EvalAttestationVerificationError> {
        let expected_signature = format!("offline:{}", attestation.payload_digest);
        if attestation.signature != expected_signature {
            return Err(EvalAttestationVerificationError::SignatureInvalid {
                reason: "offline fixture signature mismatch".to_string(),
            });
        }
        Ok(KeylessOidcVerification {
            runner_identity: attestation.claims.runner_identity.clone(),
            audience: "harness-eval".to_string(),
            subjects: vec![
                "repo:majiayu000/harness".to_string(),
                format!("commit:{}", attestation.claims.commit),
            ],
        })
    }

    fn fixture_attestation() -> EvalRunAttestation {
        serde_json::from_str(include_str!("fixtures/eval_run_attestation.json"))
            .expect("fixture should deserialize")
    }

    fn fixture_expected() -> EvalRunAttestationExpected {
        let attestation = fixture_attestation();
        EvalRunAttestationExpected {
            subjects: vec![
                "repo:majiayu000/harness".to_string(),
                format!("commit:{}", attestation.claims.commit),
            ],
            audience: "harness-eval".to_string(),
            claims: attestation.claims,
        }
    }

    #[test]
    fn offline_fixture_verifies_signature_identity_audience_subjects_and_claims() {
        let attestation = fixture_attestation();
        let expected = fixture_expected();

        let verified = verify_eval_run_attestation(&attestation, &expected, &OfflineProvider)
            .expect("offline fixture should verify");

        assert_eq!(verified.provider, "offline-oidc");
        assert_eq!(
            verified.claims.stack_id,
            "agent-stack:majiayu000/harness:eval:core"
        );
        assert_eq!(verified.audience, "harness-eval");
        assert_eq!(verified.subjects, expected.subjects);
    }

    #[test]
    fn fixture_payload_digest_binds_every_attested_claim() {
        let mut attestation = fixture_attestation();
        attestation.claims.decision = EvalAttestationDecision::Rejected;

        let error =
            verify_eval_run_attestation(&attestation, &fixture_expected(), &OfflineProvider)
                .expect_err("modified claims should invalidate payload digest");

        assert!(matches!(
            error,
            EvalAttestationVerificationError::PayloadDigestMismatch { .. }
        ));
    }

    #[test]
    fn verifier_rejects_signature_identity_audience_and_subject_mismatch() {
        let mut attestation = fixture_attestation();
        attestation.signature = "offline:not-the-digest".to_string();
        let signature_error =
            verify_eval_run_attestation(&attestation, &fixture_expected(), &OfflineProvider)
                .expect_err("bad signature should fail");
        assert!(matches!(
            signature_error,
            EvalAttestationVerificationError::SignatureInvalid { .. }
        ));

        let attestation = fixture_attestation();
        let mut wrong_identity = fixture_expected();
        wrong_identity.claims.runner_identity = "repo:other/workflow".to_string();
        let identity_error =
            verify_eval_run_attestation(&attestation, &wrong_identity, &OfflineProvider)
                .expect_err("wrong identity should fail");
        assert!(matches!(
            identity_error,
            EvalAttestationVerificationError::ClaimMismatch {
                field: "runner_identity",
                ..
            }
        ));

        let provider_identity_error =
            verify_eval_run_attestation(&attestation, &fixture_expected(), &WrongIdentityProvider)
                .expect_err("provider identity mismatch should fail");
        assert!(matches!(
            provider_identity_error,
            EvalAttestationVerificationError::IdentityMismatch { .. }
        ));

        let mut wrong_audience = fixture_expected();
        wrong_audience.audience = "other-audience".to_string();
        let audience_error =
            verify_eval_run_attestation(&attestation, &wrong_audience, &OfflineProvider)
                .expect_err("wrong audience should fail");
        assert!(matches!(
            audience_error,
            EvalAttestationVerificationError::AudienceMismatch { .. }
        ));

        let mut wrong_subject = fixture_expected();
        wrong_subject.subjects = vec!["repo:majiayu000/harness".to_string()];
        let subject_error =
            verify_eval_run_attestation(&attestation, &wrong_subject, &OfflineProvider)
                .expect_err("wrong subject should fail");
        assert!(matches!(
            subject_error,
            EvalAttestationVerificationError::SubjectsMismatch { .. }
        ));
    }

    #[test]
    fn unsigned_runs_classify_below_verified_and_never_approved() {
        let expected = fixture_expected();
        let unsigned = classify_eval_run_attestation(None, &expected, &OfflineProvider);
        let verified = classify_eval_run_attestation(
            Some(&fixture_attestation()),
            &expected,
            &OfflineProvider,
        );

        assert!(unsigned.trust < verified.trust);
        assert_eq!(unsigned.trust, EvalAttestationTrust::Unsigned);
        assert_eq!(verified.trust, EvalAttestationTrust::Verified);
        assert!(!unsigned.is_approved());
        assert!(verified.is_approved());
    }
}
