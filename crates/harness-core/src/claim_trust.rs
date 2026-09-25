use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClaimTrustLevel {
    SelfDeclared,
    RepositoryObserved,
    RuntimeObserved,
    RunnerObserved,
    Reexecuted,
    CryptographicallyAttested,
    HumanApproved,
}

impl ClaimTrustLevel {
    pub const ALL: &'static [Self] = &[
        Self::SelfDeclared,
        Self::RepositoryObserved,
        Self::RuntimeObserved,
        Self::RunnerObserved,
        Self::Reexecuted,
        Self::CryptographicallyAttested,
        Self::HumanApproved,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfDeclared => "self_declared",
            Self::RepositoryObserved => "repository_observed",
            Self::RuntimeObserved => "runtime_observed",
            Self::RunnerObserved => "runner_observed",
            Self::Reexecuted => "reexecuted",
            Self::CryptographicallyAttested => "cryptographically_attested",
            Self::HumanApproved => "human_approved",
        }
    }

    pub const fn requires_proof_metadata(self) -> bool {
        !matches!(self, Self::SelfDeclared)
    }

    pub fn satisfies(self, required: Self) -> bool {
        self >= required
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClaimProvenanceSource {
    AgentStatement,
    RepositoryRead,
    RuntimeObservation,
    RunnerObservation,
    Reexecution,
    CryptographicSignature,
    HumanApproval,
}

impl ClaimProvenanceSource {
    pub const fn for_trust(trust: ClaimTrustLevel) -> Self {
        match trust {
            ClaimTrustLevel::SelfDeclared => Self::AgentStatement,
            ClaimTrustLevel::RepositoryObserved => Self::RepositoryRead,
            ClaimTrustLevel::RuntimeObserved => Self::RuntimeObservation,
            ClaimTrustLevel::RunnerObserved => Self::RunnerObservation,
            ClaimTrustLevel::Reexecuted => Self::Reexecution,
            ClaimTrustLevel::CryptographicallyAttested => Self::CryptographicSignature,
            ClaimTrustLevel::HumanApproved => Self::HumanApproval,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ClaimProof {
    RepositoryObserved {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_sha256: Option<String>,
    },
    RuntimeObserved {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_ref: Option<String>,
    },
    RunnerObserved {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_sha256: Option<String>,
    },
    Reexecuted {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_sha256: Option<String>,
    },
    CryptographicAttestation {
        payload_sha256: String,
        signature: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_id: Option<String>,
    },
    HumanApproval {
        approver: String,
        approval_ref: String,
    },
}

impl ClaimProof {
    pub fn trust_level(&self) -> ClaimTrustLevel {
        match self {
            Self::RepositoryObserved { .. } => ClaimTrustLevel::RepositoryObserved,
            Self::RuntimeObserved { .. } => ClaimTrustLevel::RuntimeObserved,
            Self::RunnerObserved { .. } => ClaimTrustLevel::RunnerObserved,
            Self::Reexecuted { .. } => ClaimTrustLevel::Reexecuted,
            Self::CryptographicAttestation { .. } => ClaimTrustLevel::CryptographicallyAttested,
            Self::HumanApproval { .. } => ClaimTrustLevel::HumanApproved,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::RepositoryObserved {
                source,
                content_sha256,
            } => {
                validate_nonempty_claim_metadata(source, "repository observed source")?;
                validate_optional_claim_metadata(content_sha256, "repository content_sha256")
            }
            Self::RuntimeObserved { source, event_ref } => {
                validate_nonempty_claim_metadata(source, "runtime observed source")?;
                validate_optional_claim_metadata(event_ref, "runtime event_ref")
            }
            Self::RunnerObserved {
                source,
                command,
                output_sha256,
            } => {
                validate_nonempty_claim_metadata(source, "runner observed source")?;
                validate_optional_claim_metadata(command, "runner command")?;
                validate_optional_claim_metadata(output_sha256, "runner output_sha256")
            }
            Self::Reexecuted {
                command,
                output_sha256,
            } => {
                validate_nonempty_claim_metadata(command, "reexecuted command")?;
                validate_optional_claim_metadata(output_sha256, "reexecuted output_sha256")
            }
            Self::CryptographicAttestation {
                payload_sha256,
                signature,
                key_id,
            } => {
                validate_nonempty_claim_metadata(
                    payload_sha256,
                    "cryptographic attestation payload_sha256",
                )?;
                validate_nonempty_claim_metadata(signature, "cryptographic attestation signature")?;
                validate_optional_claim_metadata(key_id, "cryptographic attestation key_id")
            }
            Self::HumanApproval {
                approver,
                approval_ref,
            } => {
                validate_nonempty_claim_metadata(approver, "human approval approver")?;
                validate_nonempty_claim_metadata(approval_ref, "human approval approval_ref")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaimProvenance {
    pub source: ClaimProvenanceSource,
    pub trust: ClaimTrustLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<ClaimProof>,
}

impl Default for ClaimProvenance {
    fn default() -> Self {
        Self::self_declared()
    }
}

impl ClaimProvenance {
    pub const fn self_declared() -> Self {
        Self {
            source: ClaimProvenanceSource::AgentStatement,
            trust: ClaimTrustLevel::SelfDeclared,
            proof: None,
        }
    }

    pub fn with_trust(trust: ClaimTrustLevel, proof: Option<ClaimProof>) -> Self {
        Self {
            source: ClaimProvenanceSource::for_trust(trust),
            trust,
            proof,
        }
    }

    pub fn repository_observed(source: impl Into<String>, content_sha256: Option<String>) -> Self {
        Self::with_trust(
            ClaimTrustLevel::RepositoryObserved,
            Some(ClaimProof::RepositoryObserved {
                source: source.into(),
                content_sha256,
            }),
        )
    }

    pub fn runtime_observed(source: impl Into<String>, event_ref: Option<String>) -> Self {
        Self::with_trust(
            ClaimTrustLevel::RuntimeObserved,
            Some(ClaimProof::RuntimeObserved {
                source: source.into(),
                event_ref,
            }),
        )
    }

    pub fn runner_observed(
        source: impl Into<String>,
        command: Option<String>,
        output_sha256: Option<String>,
    ) -> Self {
        Self::with_trust(
            ClaimTrustLevel::RunnerObserved,
            Some(ClaimProof::RunnerObserved {
                source: source.into(),
                command,
                output_sha256,
            }),
        )
    }

    pub fn reexecuted(command: impl Into<String>, output_sha256: Option<String>) -> Self {
        Self::with_trust(
            ClaimTrustLevel::Reexecuted,
            Some(ClaimProof::Reexecuted {
                command: command.into(),
                output_sha256,
            }),
        )
    }

    pub fn cryptographically_attested(
        payload_sha256: impl Into<String>,
        signature: impl Into<String>,
        key_id: Option<String>,
    ) -> Self {
        Self::with_trust(
            ClaimTrustLevel::CryptographicallyAttested,
            Some(ClaimProof::CryptographicAttestation {
                payload_sha256: payload_sha256.into(),
                signature: signature.into(),
                key_id,
            }),
        )
    }

    pub fn human_approved(approver: impl Into<String>, approval_ref: impl Into<String>) -> Self {
        Self::with_trust(
            ClaimTrustLevel::HumanApproved,
            Some(ClaimProof::HumanApproval {
                approver: approver.into(),
                approval_ref: approval_ref.into(),
            }),
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        let expected_source = ClaimProvenanceSource::for_trust(self.trust);
        if self.source != expected_source {
            return Err(format!(
                "claim provenance source {:?} does not match trust {}",
                self.source,
                self.trust.as_str()
            ));
        }
        match (&self.proof, self.trust.requires_proof_metadata()) {
            (None, true) => Err(format!(
                "claim trust {} requires proof metadata",
                self.trust.as_str()
            )),
            (Some(proof), _) => {
                if proof.trust_level() != self.trust {
                    return Err(format!(
                        "claim proof {:?} does not match trust {}",
                        proof.trust_level(),
                        self.trust.as_str()
                    ));
                }
                proof.validate()
            }
            (None, false) => Ok(()),
        }
    }
}

fn validate_nonempty_claim_metadata(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_optional_claim_metadata(value: &Option<String>, field: &str) -> Result<(), String> {
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        Err(format!("{field} must not be empty when present"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_trust_vocabulary_is_closed_and_ordered() {
        let actual = ClaimTrustLevel::ALL
            .iter()
            .map(|trust| {
                serde_json::to_value(trust)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                "self_declared",
                "repository_observed",
                "runtime_observed",
                "runner_observed",
                "reexecuted",
                "cryptographically_attested",
                "human_approved"
            ]
        );
        assert!(serde_json::from_str::<ClaimTrustLevel>("\"unsigned_attestation\"").is_err());
        assert!(ClaimTrustLevel::Reexecuted.satisfies(ClaimTrustLevel::RuntimeObserved));
        assert!(!ClaimTrustLevel::SelfDeclared.satisfies(ClaimTrustLevel::RuntimeObserved));
    }

    #[test]
    fn claim_trust_stronger_levels_require_matching_proof_metadata() {
        let missing_proof =
            ClaimProvenance::with_trust(ClaimTrustLevel::CryptographicallyAttested, None);
        assert!(missing_proof
            .validate()
            .unwrap_err()
            .contains("cryptographically_attested requires proof metadata"));

        let unsigned_attestation =
            ClaimProvenance::cryptographically_attested("sha256:payload", "  ", None);
        assert!(unsigned_attestation
            .validate()
            .unwrap_err()
            .contains("signature must not be empty"));

        let inferred_human_approval = ClaimProvenance::human_approved("approver", " ");
        assert!(inferred_human_approval
            .validate()
            .unwrap_err()
            .contains("approval_ref must not be empty"));
    }

    #[test]
    fn claim_trust_rejects_mismatched_provenance_source_and_proof() {
        let mut runtime = ClaimProvenance::runtime_observed("runtime", Some("event-1".to_string()));
        runtime.source = ClaimProvenanceSource::HumanApproval;
        assert!(runtime
            .validate()
            .unwrap_err()
            .contains("does not match trust runtime_observed"));

        let mismatched_proof = ClaimProvenance::with_trust(
            ClaimTrustLevel::RuntimeObserved,
            Some(ClaimProof::HumanApproval {
                approver: "maintainer".to_string(),
                approval_ref: "ticket-1".to_string(),
            }),
        );
        assert!(mismatched_proof
            .validate()
            .unwrap_err()
            .contains("does not match trust runtime_observed"));
    }
}
