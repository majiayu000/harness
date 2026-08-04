use super::data_provenance::{workflow_data_pointer, DataProvenance, WorkflowDataProvenance};
use super::model::WorkflowInstance;
use anyhow::Context;
use serde_json::{Map, Value};

/// One provenance-bearing top-level workflow data mutation.
///
/// Reducers often persist several related fields together. Applying a batch
/// through this type keeps the data and its provenance sidecar in one
/// in-memory commit: validation happens on clones before either field changes.
#[derive(Debug, Clone)]
pub struct WorkflowDataWrite {
    pub field: String,
    pub value: Option<Value>,
    pub provenance: DataProvenance,
}

impl WorkflowDataWrite {
    pub fn set(
        field: impl Into<String>,
        value: impl Into<Value>,
        provenance: DataProvenance,
    ) -> Self {
        Self {
            field: field.into(),
            value: Some(value.into()),
            provenance,
        }
    }

    pub fn remove(field: impl Into<String>, provenance: DataProvenance) -> Self {
        Self {
            field: field.into(),
            value: None,
            provenance,
        }
    }
}

impl WorkflowInstance {
    /// Row-level provenance invariant: `workflow.data` must be fully covered
    /// by a sidecar whose digests match the data being persisted.
    ///
    /// Every path that writes a workflow row must enforce this, including
    /// test-only writers that deliberately bypass lifecycle validation. A
    /// sidecar that disagrees with its own data is a corrupt row, and a
    /// corrupt row is never what a fixture is trying to construct.
    pub fn validate_data_provenance(&self) -> anyhow::Result<()> {
        self.data_provenance
            .as_ref()
            .context("workflow.data persistence requires a provenance sidecar")?
            .validate_persisted_data(&self.data)
    }

    /// Adopt another instance's workflow data together with the provenance
    /// sidecar that describes it.
    ///
    /// Copying `data` alone leaves this instance carrying a sidecar that
    /// describes different bytes, which fails closed at persistence. The two
    /// fields are one value and must move together.
    pub fn adopt_classified_data_from(&mut self, source: &WorkflowInstance) -> anyhow::Result<()> {
        source
            .validate_data_provenance()
            .context("adopted workflow data must carry a coherent provenance sidecar")?;
        self.data = source.data.clone();
        self.data_provenance = source.data_provenance.clone();
        Ok(())
    }

    /// Fixture builder that seeds workflow data already classified as
    /// server-authored.
    ///
    /// The name carries the provenance claim so a fixture can never seed
    /// agent- or externally-authored data while presenting it to the taint
    /// fence as server-authored. Fixtures that model untrusted data must use
    /// [`Self::with_classified_data`] or [`Self::with_data_field_provenance`]
    /// with the provenance they actually mean.
    ///
    /// This is not a migration boundary: only rows loaded from durable
    /// storage can be grandfathered as legacy data.
    pub fn with_server_data(self, data: Value) -> Self {
        self.with_classified_data(data, DataProvenance::Server)
    }

    pub fn with_classified_data(mut self, data: Value, provenance: DataProvenance) -> Self {
        self.replace_classified_data(data, provenance);
        self
    }

    pub fn replace_classified_data(&mut self, data: Value, provenance: DataProvenance) {
        self.data = data;
        let mut sidecar = WorkflowDataProvenance::new().with_entry("", provenance);
        sidecar
            .refresh_value_digests(&self.data)
            .expect("serializing serde_json::Value cannot fail");
        self.data_provenance = Some(sidecar);
    }

    /// Replace the whole `workflow.data` document, classifying each field the
    /// write actually authored.
    ///
    /// Callers typically stage a new document by cloning the current data and
    /// editing a few fields, so most fields arrive unchanged. An unchanged
    /// field was not authored by this write, and reclassifying it would let a
    /// caller's default arm silently promote grandfathered or externally
    /// authored history to trusted server data — the value would then be
    /// rendered outside the untrusted fence. Unchanged fields therefore keep
    /// whatever coverage they already had, and the migration boundary carries
    /// forward so later readers can still tell pre-provenance history from a
    /// post-deployment writer defect.
    pub fn replace_data_with_field_provenance(
        &mut self,
        data: Value,
        mut provenance_for: impl FnMut(&str) -> DataProvenance,
    ) -> anyhow::Result<()> {
        let object = data
            .as_object()
            .context("workflow instance data must be a JSON object")?;
        let existing = self.data_provenance.clone();
        let mut provenance = WorkflowDataProvenance::new();
        if let Some(migrated_at) = existing.as_ref().and_then(|existing| existing.migrated_at) {
            provenance.migrated_at = Some(migrated_at);
        }
        for (field, value) in object {
            let pointer = workflow_data_pointer("", field);
            let unchanged = self
                .data
                .pointer(&pointer)
                .is_some_and(|current| current == value);
            if unchanged
                && existing.as_ref().is_some_and(|existing| {
                    existing.carry_over_coverage_into(&pointer, &mut provenance)
                })
            {
                continue;
            }
            provenance.classify(pointer, provenance_for(field.as_str()));
        }
        provenance.refresh_value_digests(&data)?;
        provenance.validate_persisted_data(&data)?;
        self.data = data;
        self.data_provenance = Some(provenance);
        Ok(())
    }

    pub fn with_data_field_provenance(
        mut self,
        data: Value,
        provenance_for: impl FnMut(&str) -> DataProvenance,
    ) -> Self {
        self.replace_data_with_field_provenance(data, provenance_for)
            .expect("workflow field provenance requires JSON object data");
        self
    }

    pub fn apply_data_writes(
        &mut self,
        writes: impl IntoIterator<Item = WorkflowDataWrite>,
    ) -> anyhow::Result<()> {
        let mut provenance = self
            .data_provenance
            .clone()
            .context("workflow.data writes require a provenance sidecar")?;
        provenance.validate_persisted_data(&self.data)?;
        let mut data = self.data.clone();
        if data.is_null() {
            data = Value::Object(Map::new());
        }
        let object = data
            .as_object_mut()
            .context("workflow instance data must be a JSON object")?;
        for write in writes {
            validate_field_name(&write.field)?;
            let pointer = workflow_data_pointer("", &write.field);
            match write.value {
                Some(value) => {
                    object.insert(write.field, value);
                }
                None => {
                    object.remove(&write.field);
                    provenance.remove_classification(&pointer);
                    continue;
                }
            }
            provenance.classify(pointer, write.provenance);
        }

        provenance.refresh_value_digests(&data)?;
        self.data = data;
        self.data_provenance = Some(provenance);
        Ok(())
    }

    pub fn set_data_field(
        &mut self,
        field: impl Into<String>,
        value: impl Into<Value>,
        provenance: DataProvenance,
    ) -> anyhow::Result<()> {
        self.apply_data_writes([WorkflowDataWrite::set(field, value, provenance)])
    }

    pub fn remove_data_field(
        &mut self,
        field: impl Into<String>,
        provenance: DataProvenance,
    ) -> anyhow::Result<()> {
        self.apply_data_writes([WorkflowDataWrite::remove(field, provenance)])
    }
}

fn validate_field_name(field: &str) -> anyhow::Result<()> {
    if field.is_empty() {
        anyhow::bail!("workflow data field name must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::WorkflowSubject;
    use serde_json::json;

    fn instance() -> WorkflowInstance {
        WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task"),
        )
    }

    #[test]
    fn first_classified_write_migrates_legacy_fields_atomically() {
        let data = json!({"historical": "value"});
        let mut workflow = instance().with_server_data(data.clone());
        workflow.data_provenance = Some(
            WorkflowDataProvenance::migrated_from_persisted_data(&data)
                .expect("persisted migration"),
        );
        workflow
            .set_data_field(
                "continuation",
                json!({"attempt": 2}),
                DataProvenance::External,
            )
            .expect("classified write");

        let sidecar = workflow.data_provenance.expect("sidecar");
        assert!(sidecar.migrated_at.is_some());
        assert!(sidecar.is_legacy("/historical"));
        assert_eq!(
            sidecar.provenance_for("/continuation"),
            Some(DataProvenance::External)
        );
    }

    #[test]
    fn whole_document_replacement_does_not_launder_unchanged_legacy_fields() {
        // A pre-provenance row: everything it carries is grandfathered.
        let persisted = json!({
            "additional_prompt": "hostile historical instruction",
            "task_id": "task-1",
        });
        let mut workflow = instance().with_server_data(persisted.clone());
        workflow.data_provenance = Some(
            WorkflowDataProvenance::migrated_from_persisted_data(&persisted)
                .expect("persisted migration"),
        );
        let migrated_at = workflow
            .data_provenance
            .as_ref()
            .and_then(|sidecar| sidecar.migrated_at)
            .expect("migration boundary timestamp");

        // A later transition stages a new document by cloning the current data
        // and touching one field. Its mapping would call everything unknown
        // `Server`.
        let mut staged = workflow.data.clone();
        staged["last_decision"] = json!("address_feedback");
        workflow
            .replace_data_with_field_provenance(staged, |_| DataProvenance::Server)
            .expect("classified replacement");

        let sidecar = workflow.data_provenance.as_ref().expect("sidecar");
        // The untouched historical field must stay fenced, not become trusted
        // server data just because it rode along in the staged document.
        assert!(
            sidecar.is_legacy("/additional_prompt"),
            "an unchanged grandfathered field must not be reclassified"
        );
        assert_eq!(sidecar.provenance_for("/additional_prompt"), None);
        assert!(sidecar.is_legacy("/task_id"));
        // Only the field this write authored is newly classified.
        assert_eq!(
            sidecar.provenance_for("/last_decision"),
            Some(DataProvenance::Server)
        );
        // The boundary itself survives, so later readers can still tell
        // pre-provenance history from a writer defect.
        assert_eq!(sidecar.migrated_at, Some(migrated_at));
    }

    #[test]
    fn whole_document_replacement_reclassifies_fields_the_write_changed() {
        let persisted = json!({"additional_prompt": "old", "task_id": "task-1"});
        let mut workflow = instance().with_server_data(persisted.clone());
        workflow.data_provenance = Some(
            WorkflowDataProvenance::migrated_from_persisted_data(&persisted)
                .expect("persisted migration"),
        );

        let mut staged = workflow.data.clone();
        staged["additional_prompt"] = json!("rewritten by this transition");
        workflow
            .replace_data_with_field_provenance(staged, |field| match field {
                "additional_prompt" => DataProvenance::External,
                _ => DataProvenance::Server,
            })
            .expect("classified replacement");

        let sidecar = workflow.data_provenance.as_ref().expect("sidecar");
        // This write authored the value, so it leaves the legacy boundary and
        // takes the writer's classification.
        assert!(!sidecar.is_legacy("/additional_prompt"));
        assert_eq!(
            sidecar.provenance_for("/additional_prompt"),
            Some(DataProvenance::External)
        );
        assert!(sidecar.is_legacy("/task_id"));
    }

    #[test]
    fn null_persisted_data_crosses_the_legacy_boundary() {
        // An older serialized instance that omitted `data` deserializes to
        // null. It must still be readable, writable, and repairable.
        let provenance = WorkflowDataProvenance::migrated_from_persisted_data(&Value::Null)
            .expect("null migration");
        provenance
            .validate_persisted_data(&Value::Null)
            .expect("a migrated null document must be persistable");

        let mut workflow = instance();
        workflow.data = Value::Null;
        workflow.data_provenance = Some(provenance);
        workflow
            .set_data_field("repo", json!("owner/repo"), DataProvenance::Server)
            .expect("a null document must accept its first classified write");

        assert_eq!(workflow.data["repo"], "owner/repo");
        workflow
            .validate_data_provenance()
            .expect("the repaired document is persistable");
    }

    #[test]
    fn rejected_batch_does_not_change_data_or_sidecar() {
        let mut workflow =
            instance().with_classified_data(json!("not-an-object"), DataProvenance::Server);
        let before = workflow.clone();
        let error = workflow
            .set_data_field("field", json!(true), DataProvenance::Server)
            .expect_err("non-object write should fail");

        assert!(error.to_string().contains("must be a JSON object"));
        assert_eq!(workflow, before);
    }

    #[test]
    fn raw_overwrite_of_classified_field_is_rejected() {
        let mut workflow =
            instance().with_classified_data(json!({"field": "before"}), DataProvenance::Server);
        workflow.data["field"] = json!("after");

        let error = workflow
            .set_data_field("other", json!(true), DataProvenance::Server)
            .expect_err("raw mutation must fail closed");

        assert!(error
            .to_string()
            .contains("changed outside the classified write API"));
    }
}
