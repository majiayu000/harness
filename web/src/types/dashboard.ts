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

export interface DashboardPayload {
  projects: DashboardProject[];
  runtime_hosts: DashboardRuntimeHost[];
  llm_metrics: DashboardLlmMetrics;
  global: DashboardGlobal;
}
