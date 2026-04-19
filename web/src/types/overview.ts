export interface OverviewWindow {
  hours: number;
  since: string;
  now: string;
}

export interface OverviewKpiWorktrees {
  used: number;
  total: number;
}

export interface OverviewKpi {
  active_tasks: number;
  merged_24h: number;
  avg_review_score: number | null;
  grade: string | null;
  rule_fail_rate_pct: number;
  tokens_24h: number | null;
  worktrees: OverviewKpiWorktrees;
}

export interface OverviewDistribution {
  queued: number;
  running: number;
  review: number;
  merged: number;
  failed: number;
}

export interface OverviewThroughputSeries {
  project: string;
  values: number[];
}

export interface OverviewThroughput {
  hours: string[];
  series: OverviewThroughputSeries[];
}

export interface OverviewProject {
  id: string;
  root: string;
  running: number;
  queued: number;
  done: number;
  failed: number;
  merged_24h: number;
  trend: number[];
  avg_score: number | null;
  worktrees: string | null;
  tokens_24h: number | null;
  agents: string[];
  latest_pr: string | null;
}

export interface OverviewRuntime {
  id: string;
  display_name: string;
  capabilities: string[];
  online: boolean;
  last_heartbeat_at: string;
  active_leases: number;
  watched_projects: number;
  cpu_pct: number | null;
  ram_pct: number | null;
  tokens_24h: number | null;
}

export interface OverviewHeatmapRow {
  label: string;
  intensity: number[];
}

export interface OverviewHeatmap {
  bucket_minutes: number;
  rows: OverviewHeatmapRow[];
}

export interface OverviewFeedEntry {
  ts: string;
  ago: string;
  kind: string;
  tool: string;
  body: string;
  level: "ok" | "warn" | "err" | "";
  project: string;
}

export interface OverviewAlert {
  level: "ok" | "warn" | "err";
  msg: string;
  sub: string;
  ts: string | null;
}

export interface OverviewGlobal {
  uptime_secs: number;
  running: number;
  queued: number;
  done: number;
  failed: number;
  max_concurrent: number;
}

export interface OverviewPayload {
  window: OverviewWindow;
  kpi: OverviewKpi;
  distribution: OverviewDistribution;
  throughput: OverviewThroughput;
  projects: OverviewProject[];
  runtimes: OverviewRuntime[];
  heatmap: OverviewHeatmap;
  feed: OverviewFeedEntry[];
  alerts: OverviewAlert[];
  global: OverviewGlobal;
}
