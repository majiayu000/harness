export interface OperatorMonitorPayload {
  generated_at: string;
  sample_limit: number;
  health: OperatorHealth;
  activity: OperatorActivity;
  operator_actions: OperatorAction[];
  failures: FailureGroup[];
  worktrees: OperatorWorktreeSummary;
}

export interface OperatorHealth {
  status: "ok" | "degraded";
  degraded_subsystems: string[];
  runtime_log_state: "disabled" | "enabled" | "degraded";
  runtime_log_path: string | null;
  uptime_secs: number;
  runtime_hosts_online: number;
  runtime_hosts_total: number;
}

export interface OperatorActivity {
  runtime_workflows: RuntimeWorkflowCounts;
  legacy_queue: LegacyQueueCounts;
  by_source: SourceActivity[];
}

export interface RuntimeWorkflowCounts {
  pending: number;
  running: number;
  review: number;
  awaiting_dependencies: number;
  ready_to_merge: number;
  blocked: number;
  failed: number;
  done: number;
  other: number;
}

export interface LegacyQueueCounts {
  queued: number;
  running: number;
  stalled: number;
  failed: number;
  done: number;
}

export interface SourceActivity {
  source: string;
  pending: number;
  running: number;
  review: number;
  blocked: number;
  failed: number;
  ready_to_merge: number;
}

export interface OperatorAction {
  kind: "ready_to_merge" | "awaiting_feedback" | "blocked" | string;
  repo: string | null;
  issue: number | null;
  pr: number | null;
  task_id: string | null;
  workflow_id: string;
  state: string;
  age_secs: number;
  url: string | null;
  evidence_url: string | null;
  next_action: string;
  source: string;
}

export interface FailureGroup {
  family: string;
  severity: "info" | "warn" | "error" | "critical";
  message: string;
  first_seen: string | null;
  last_seen: string | null;
  count: number;
  repo: string | null;
  task_id: string | null;
  retryable: boolean;
  next_action: string;
}

export interface OperatorWorktreeSummary {
  used: number;
  capacity: number;
  stale: number | null;
  metrics_state: "unavailable" | "available";
  cards: OperatorWorktreeCard[];
}

export interface OperatorWorktreeCard {
  task_id: string;
  branch: string;
  workspace_path: string;
  path_short: string;
  source_repo: string;
  repo: string | null;
  runtime_workflow_id: string | null;
  status: string;
  phase: string;
  description: string | null;
  turn: number;
  max_turns: number | null;
  created_at: string;
  duration_secs: number;
  pr_url: string | null;
  project: string | null;
}
