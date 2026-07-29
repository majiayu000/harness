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
    /// Preserve the historical builder API while making its legacy boundary
    /// explicit. This method does not create a migration boundary: only rows
    /// loaded from durable storage can be grandfathered as legacy data.
    /// Production callers must classify the value before persistence.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
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

    pub fn replace_data_with_field_provenance(
        &mut self,
        data: Value,
        mut provenance_for: impl FnMut(&str) -> DataProvenance,
    ) -> anyhow::Result<()> {
        let object = data
            .as_object()
            .context("workflow instance data must be a JSON object")?;
        let mut provenance = WorkflowDataProvenance::new();
        for field in object.keys() {
            provenance.classify(
                workflow_data_pointer("", field),
                provenance_for(field.as_str()),
            );
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
        let mut workflow = instance().with_data(data.clone());
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
