export interface UsageWindow {
  hours: number;
  since: string;
  now: string;
}

export interface UsageCostConfig {
  currency: "USD";
  configured: boolean;
  source: string;
  missing_model_count: number;
  message: string;
}

export interface UsageSummary {
  total_tokens: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
  request_count: number;
  estimated_cost_usd: number | null;
  running_agent_invocations: number;
  active_leased_agent_invocations: number;
  expired_or_missing_lease_agent_invocations: number;
  pending_agent_invocations: number;
  stale_agent_invocations: number;
  high_burn_invocations: number;
  external_agent_processes: number;
}

export interface UsageGroup {
  name: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
  total_tokens: number;
  request_count: number;
  estimated_cost_usd: number | null;
  cost_confidence: string;
}

export interface LocalUsageModelSummary {
  model: string;
  estimated_cost_usd: number | null;
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
  total_tokens: number;
}

export interface LocalUsageSourceSummary {
  source: string;
  display_name: string;
  attribution: "global_local_source_logs";
  status: "available" | "unavailable";
  since: string;
  until: string;
  currency: string;
  estimated_cost_usd: number | null;
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
  total_tokens: number;
  period_count: number;
  model_count: number;
  models: LocalUsageModelSummary[];
  cost_confidence: string;
  elapsed_ms: number;
  error: string | null;
}

export interface AgentInvocation {
  agent_invocation_id: string;
  source: "workflow_runtime";
  runtime_job_id: string;
  command_id: string;
  workflow_id: string;
  workflow_definition: string;
  workflow_state: string;
  subject_type: string;
  subject_key: string;
  repo: string | null;
  project: string | null;
  issue_number: number | null;
  pr_number: number | null;
  task_id: string | null;
  activity: string;
  status: string;
  command_status: string;
  agent_runtime: string;
  runtime_profile: string;
  model: string | null;
  reasoning_effort: string | null;
  lease_owner: string | null;
  lease_expires_at: string | null;
  lease_state: string | null;
  in_flight_model_turn: boolean;
  latest_runtime_event_type: string | null;
  last_runtime_observation_at: string | null;
  stale: boolean;
  age_secs: number;
  updated_age_secs: number;
  burn_level: "low" | "medium" | "high";
  cost_confidence: string;
}

export interface AgentProcess {
  pid: number;
  name: string;
  agent: "codex" | "claude";
  age_secs: number;
  cpu_pct: number;
  memory_bytes: number;
  command_label: string;
}

export interface ActiveCount {
  name: string;
  running: number;
  active_leased: number;
  expired_or_missing_lease: number;
  pending: number;
  high_burn: number;
}

export interface UsageDiagnostics {
  runtime_store_available: boolean;
  token_source: string;
  active_cost_confidence: string;
  process_source: string;
}

export interface UsageMonitorPayload {
  window: UsageWindow;
  cost: UsageCostConfig;
  summary: UsageSummary;
  tokens_by_agent: UsageGroup[];
  tokens_by_project: UsageGroup[];
  tokens_by_model: UsageGroup[];
  agent_invocations: AgentInvocation[];
  external_agent_processes: AgentProcess[];
  local_usage_sources: LocalUsageSourceSummary[];
  active_by_repo: ActiveCount[];
  active_by_activity: ActiveCount[];
  diagnostics: UsageDiagnostics;
}
