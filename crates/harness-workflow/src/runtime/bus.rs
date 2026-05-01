use super::model::{
    ActivityResult, RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind, WorkflowCommand,
    WorkflowDecisionRecord, WorkflowEvent, WorkflowInstance,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct InMemoryWorkflowBus {
    instances: BTreeMap<String, WorkflowInstance>,
    events_by_workflow: BTreeMap<String, Vec<WorkflowEvent>>,
    decisions: Vec<WorkflowDecisionRecord>,
    commands: BTreeMap<String, WorkflowCommand>,
    runtime_jobs: BTreeMap<String, RuntimeJob>,
    runtime_events: BTreeMap<String, Vec<RuntimeEvent>>,
}

impl InMemoryWorkflowBus {
    pub fn insert_instance(&mut self, instance: WorkflowInstance) {
        self.instances.insert(instance.id.clone(), instance);
    }

    pub fn get_instance(&self, workflow_id: &str) -> Option<&WorkflowInstance> {
        self.instances.get(workflow_id)
    }

    pub fn update_instance(&mut self, instance: WorkflowInstance) {
        self.instances.insert(instance.id.clone(), instance);
    }

    pub fn append_event(
        &mut self,
        workflow_id: impl Into<String>,
        event_type: impl Into<String>,
        source: impl Into<String>,
        payload: Value,
    ) -> WorkflowEvent {
        let workflow_id = workflow_id.into();
        let sequence = self
            .events_by_workflow
            .get(&workflow_id)
            .map_or(1, |events| events.len() as u64 + 1);
        let event = WorkflowEvent::new(workflow_id.clone(), sequence, event_type, source)
            .with_payload(payload);
        self.events_by_workflow
            .entry(workflow_id)
            .or_default()
            .push(event.clone());
        event
    }

    pub fn events_for(&self, workflow_id: &str) -> Vec<WorkflowEvent> {
        self.events_by_workflow
            .get(workflow_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn record_decision(&mut self, record: WorkflowDecisionRecord) {
        self.decisions.push(record);
    }

    pub fn decisions(&self) -> &[WorkflowDecisionRecord] {
        &self.decisions
    }

    pub fn enqueue_command(&mut self, command: WorkflowCommand) -> String {
        let id = Uuid::new_v4().to_string();
        self.commands.insert(id.clone(), command);
        id
    }

    pub fn command(&self, command_id: &str) -> Option<&WorkflowCommand> {
        self.commands.get(command_id)
    }

    pub fn enqueue_runtime_job(
        &mut self,
        command_id: impl Into<String>,
        runtime_kind: RuntimeKind,
        runtime_profile: impl Into<String>,
        input: Value,
    ) -> RuntimeJob {
        let job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        self.runtime_jobs.insert(job.id.clone(), job.clone());
        job
    }

    pub fn claim_next_runtime_job(
        &mut self,
        owner: impl Into<String>,
        expires_at: DateTime<Utc>,
    ) -> Option<RuntimeJob> {
        let owner = owner.into();
        let job_id = self
            .runtime_jobs
            .iter()
            .filter(|(_, job)| job.status == RuntimeJobStatus::Pending)
            .min_by_key(|(_, job)| job.created_at)
            .map(|(id, _)| id.clone())?;

        let job = self.runtime_jobs.get_mut(&job_id)?;
        job.claim(owner, expires_at);
        Some(job.clone())
    }

    pub fn record_runtime_event(
        &mut self,
        runtime_job_id: impl Into<String>,
        event_type: impl Into<String>,
        payload: Value,
    ) -> RuntimeEvent {
        let runtime_job_id = runtime_job_id.into();
        let sequence = self
            .runtime_events
            .get(&runtime_job_id)
            .map_or(1, |events| events.len() as u64 + 1);
        let event = RuntimeEvent::new(runtime_job_id.clone(), sequence, event_type, payload);
        self.runtime_events
            .entry(runtime_job_id)
            .or_default()
            .push(event.clone());
        event
    }

    pub fn runtime_events_for(&self, runtime_job_id: &str) -> Vec<RuntimeEvent> {
        self.runtime_events
            .get(runtime_job_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn complete_runtime_job(
        &mut self,
        runtime_job_id: &str,
        result: &ActivityResult,
    ) -> anyhow::Result<RuntimeJob> {
        let job = self
            .runtime_jobs
            .get_mut(runtime_job_id)
            .ok_or_else(|| anyhow::anyhow!("runtime job not found: {runtime_job_id}"))?;
        job.complete(result)?;
        Ok(job.clone())
    }

    pub fn runtime_job(&self, runtime_job_id: &str) -> Option<&RuntimeJob> {
        self.runtime_jobs.get(runtime_job_id)
    }
}
