export interface Worktree {
  task_id: string;
  branch: string;
  workspace_path: string;
  path_short: string;
  source_repo: string;
  repo: string;
  status: string;
  phase: string;
  description: string;
  turn: number;
  max_turns: number | null;
  created_at: string;
  duration_secs: number;
  pr_url: string | null;
  project: string;
}
