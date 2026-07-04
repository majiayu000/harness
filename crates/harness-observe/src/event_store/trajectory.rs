use super::EventStore;
use crate::otel_trajectory::{ActivitySpan, AgentTurnSpan, WorkflowRootSpan};

impl EventStore {
    pub fn record_trajectory_workflow_root(&self, span: WorkflowRootSpan) {
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            pipeline.record_workflow_root_span(span);
        }
    }

    pub fn record_trajectory_activity(&self, span: ActivitySpan) {
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            pipeline.record_activity_span(span);
        }
    }

    pub fn record_trajectory_agent_turn(&self, span: AgentTurnSpan) {
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            pipeline.record_agent_turn_span(span);
        }
    }
}
