//! The definition identity a [`DecisionValidator`] speaks for (GH-1864).
//!
//! [`DecisionValidator`]: super::validator::DecisionValidator

use super::WorkflowInstance;

/// The definition identity a validator was built from.
///
/// A validator encodes which transitions are legal, so applying one that was
/// resolved from a different definition — or from a different version or
/// content hash of the same definition — authorizes transitions the instance's
/// own definition never allowed. The store re-checks this binding against the
/// row it loaded under lock, which also closes the window between a caller
/// resolving a validator and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionValidatorBinding {
    /// `None` means the validator was built from a bare allowlist and claims no
    /// definition. Such a validator cannot be shown to govern any instance, so
    /// the store refuses it rather than guessing.
    pub definition_id: Option<String>,
    /// Declarative definitions pin an exact version; built-ins do not carry
    /// one, and `None` means "this validator makes no version claim".
    pub definition_version: Option<u32>,
    /// The content hash the declarative pin resolved to. `None` for built-in
    /// definitions, which are addressed by id alone and whose instances may
    /// carry a `data.definition_hash` the validator has no claim over.
    pub definition_hash: Option<String>,
}

impl DecisionValidatorBinding {
    /// A validator that claims no definition and therefore governs nothing.
    pub fn unbound() -> Self {
        Self {
            definition_id: None,
            definition_version: None,
            definition_hash: None,
        }
    }

    /// A validator bound to a definition by id, with no version or content
    /// claim — the built-in case.
    pub fn for_definition(definition_id: &str) -> Self {
        Self {
            definition_id: Some(definition_id.to_string()),
            definition_version: None,
            definition_hash: None,
        }
    }

    /// A validator bound to the exact declarative definition a pin resolved to.
    pub fn for_declarative(definition_id: &str, definition_version: u32, hash: &str) -> Self {
        Self {
            definition_id: Some(definition_id.to_string()),
            definition_version: Some(definition_version),
            definition_hash: Some(hash.to_string()),
        }
    }

    /// Reject a validator that does not govern this instance.
    pub fn ensure_governs(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        let Some(definition_id) = self.definition_id.as_deref() else {
            anyhow::bail!(
                "decision validator is not bound to a workflow definition and cannot govern workflow `{}`",
                instance.id
            );
        };
        if definition_id != instance.definition_id {
            anyhow::bail!(
                "decision validator is bound to definition `{}` but workflow `{}` uses `{}`",
                definition_id,
                instance.id,
                instance.definition_id
            );
        }
        if let Some(version) = self.definition_version {
            if version != instance.definition_version {
                anyhow::bail!(
                    "decision validator is bound to definition `{}` version {} but workflow `{}` is at version {}",
                    definition_id,
                    version,
                    instance.id,
                    instance.definition_version
                );
            }
        }
        // Only a validator resolved through a declarative pin makes a claim
        // about content, so only it compares hashes. A built-in validator must
        // not read `data.definition_hash`: for a built-in definition that field
        // is ordinary payload, not a definition pin.
        if let Some(hash) = self.definition_hash.as_deref() {
            let pinned = instance
                .data
                .get("definition_hash")
                .and_then(serde_json::Value::as_str);
            if pinned != Some(hash) {
                anyhow::bail!(
                    "decision validator is bound to definition `{}` content hash `{}` but workflow `{}` pins `{}`",
                    definition_id,
                    hash,
                    instance.id,
                    pinned.unwrap_or("<none>")
                );
            }
        }
        Ok(())
    }
}
