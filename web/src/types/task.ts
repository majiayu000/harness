/**
 * Shape of a single row returned by `GET /tasks`. Kept minimal — only the
 * fields the dashboard kanban actually renders. Extend as consumers grow.
 */
export interface Task {
  id: string;
  status: string;
  turn: number;
  pr_url: string | null;
  error: string | null;
  source: string | null;
  parent_id: string | null;
  repo: string | null;
  description: string | null;
  created_at: string | null;
  phase: string | null;
  depends_on: string[];
  subtask_ids: string[];
  project: string | null;
}
