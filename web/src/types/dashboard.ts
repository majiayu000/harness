export interface DashboardProjectTasks {
  running: number;
  queued: number;
  done: number;
  failed: number;
}

export interface DashboardProject {
  id: string;
  root: string;
  tasks: DashboardProjectTasks;
  latest_pr: string | null;
}

export interface DashboardRuntimeHost {
  id: string;
  display_name: string;
  capabilities: string[];
  online: boolean;
  last_heartbeat_at: string;
  watched_projects: number;
  watched_project_roots: string[];
  active_leases: number;
  assignment_pressure: number;
}

export interface DashboardLlmMetrics {
  avg_turns: number | null;
  p50_turns: number | null;
  total_linter_feedback: number;
  p50_first_token_latency_ms: number | null;
}

export interface DashboardGlobal {
  running: number;
  queued: number;
  max_concurrent: number;
  uptime_secs: number;
  done: number;
  failed: number;
  latest_pr: string | null;
  grade: string | null;
  runtime_hosts_total: number;
  runtime_hosts_online: number;
}

export interface DashboardOnboarding {
  phase: "register_project" | "submit_task" | "watch_live_output" | "inspect_completion" | "complete";
  has_registered_project: boolean;
  has_submitted_task: boolean;
  has_live_output: boolean;
  has_completion_evidence: boolean;
}

export interface DashboardFunnel {
  project_registered_at: string | null;
  task_submitted_at: string | null;
  live_output_at: string | null;
  completion_evidence_at: string | null;
}

export interface DashboardPayload {
  projects: DashboardProject[];
  runtime_hosts: DashboardRuntimeHost[];
  llm_metrics: DashboardLlmMetrics;
  first_run: boolean;
  onboarding: DashboardOnboarding;
  funnel: DashboardFunnel;
  global: DashboardGlobal;
}
