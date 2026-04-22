/**
 * Shape of a single row returned by `GET /tasks`. Kept minimal — only the
 * fields the dashboard kanban actually renders. Extend as consumers grow.
 */
export interface Task {
  id: string;
  task_kind: string;
  status: string;
  turn: number;
  agent_active: boolean;
  active_phase: string | null;
  phase_started_at: string | null;
  pr_url: string | null;
  error: string | null;
  source: string | null;
  parent_id: string | null;
  external_id: string | null;
  repo: string | null;
  description: string | null;
  created_at: string | null;
  phase: string | null;
  depends_on: string[];
  subtask_ids: string[];
  project: string | null;
  workflow?: WorkflowSummary | null;
}

export interface WorkflowSummary {
  state: string;
  issue_number?: number | null;
  pr_number?: number | null;
  force_execute?: boolean;
  plan_concern?: string | null;
}
