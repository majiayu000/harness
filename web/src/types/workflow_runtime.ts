export interface WorkflowRuntimeTreePayload {
  workflows: WorkflowRuntimeTreeNode[];
  total_workflows: number;
}

export interface WorkflowRuntimeTreeNode {
  workflow: WorkflowRuntimeInstance;
  events: WorkflowRuntimeEvent[];
  decisions: WorkflowRuntimeDecisionRecord[];
  commands: WorkflowRuntimeCommandNode[];
  children: WorkflowRuntimeTreeNode[];
}

export interface WorkflowRuntimeInstance {
  id: string;
  definition_id: string;
  definition_version: number;
  state: string;
  subject: {
    subject_type: string;
    subject_key: string;
  };
  parent_workflow_id?: string | null;
  data: Record<string, unknown>;
  version: number;
  created_at: string;
  updated_at: string;
}

export interface WorkflowRuntimeEvent {
  id: string;
  workflow_id: string;
  sequence: number;
  event_type: string;
  event: Record<string, unknown>;
  source: string;
  created_at: string;
}

export interface WorkflowRuntimeDecisionRecord {
  id: string;
  workflow_id: string;
  accepted: boolean;
  rejection_reason?: string | null;
  decision: {
    decision: string;
    observed_state: string;
    next_state: string;
    reason: string;
  };
  created_at: string;
}

export interface WorkflowRuntimeCommandNode {
  id: string;
  workflow_id: string;
  decision_id?: string | null;
  status: string;
  command: {
    command_type: string;
    dedupe_key: string;
    command: Record<string, unknown>;
  };
  runtime_jobs: WorkflowRuntimeJob[];
  created_at: string;
  updated_at: string;
}

export interface WorkflowRuntimeJob {
  id: string;
  command_id: string;
  runtime_kind: string;
  runtime_profile: string;
  status: string;
  input: Record<string, unknown>;
  output?: Record<string, unknown> | null;
  error?: string | null;
  not_before?: string | null;
  runtime_event_count?: number;
  latest_runtime_event_type?: string | null;
  prompt_packet_digest?: string | null;
  created_at: string;
  updated_at: string;
}
