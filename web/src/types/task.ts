/**
 * Shape of a single row returned by `GET /tasks`. Kept minimal — only the
 * fields the dashboard kanban actually renders. Extend as consumers grow.
 */
export interface Task {
  id: string;
  task_kind: string;
  status: string;
  turn: number;
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

export interface TaskRound {
  turn: number;
  action: string;
  result: string;
  detail?: string | null;
  first_token_latency_ms?: number | null;
}

export interface TaskDetail extends Task {
  external_id?: string | null;
  rounds: TaskRound[];
  priority?: number;
}

export interface TaskPromptRecord {
  task_id: string;
  turn: number;
  phase: string | null;
  prompt: string;
  created_at: string | null;
}
