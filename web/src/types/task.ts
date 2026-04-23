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
  updated_at?: string | null;
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

export interface RegisteredProject {
  id: string;
  root: string;
  name?: string | null;
  max_concurrent?: number | null;
  default_agent?: string | null;
  active?: boolean;
  created_at?: string | null;
  task_count?: number;
}

export interface ProjectValidationResult {
  canonical_root: string;
  project_id: string;
  display_name: string;
  repo: string | null;
  errors: string[];
}

export interface RegisterProjectResponse {
  status: "created" | "existing";
  project: RegisteredProject;
  validation: ProjectValidationResult;
}

export interface CreateTaskResponse {
  task_id: string;
  status: string;
  deduped: boolean;
  task_url: string;
}

export interface TaskRound {
  turn: number;
  action: string;
  result: string;
  detail?: string | null;
  first_token_latency_ms?: number | null;
}

export interface TaskPromptRecord {
  task_id: string;
  turn: number;
  phase: string;
  prompt: string;
  created_at: string;
}

export interface TaskArtifactRecord {
  task_id: string;
  turn: number;
  artifact_type: string;
  content: string;
  created_at: string;
}

export interface TaskCheckpointSummary {
  triage_output: string | null;
  plan_output: string | null;
  pr_url: string | null;
  last_phase: string;
  updated_at: string;
}

export interface TaskCompletionSummary {
  is_terminal: boolean;
  status: string;
  has_pr: boolean;
  has_artifacts: boolean;
  has_prompts: boolean;
  pr_url: string | null;
  latest_artifact_at: string | null;
  latest_prompt_at: string | null;
  checkpoint: TaskCheckpointSummary | null;
}

export interface TaskDetail extends Task {
  rounds: TaskRound[];
  prompts: TaskPromptRecord[];
  artifacts: TaskArtifactRecord[];
  completion: TaskCompletionSummary;
  workflow: WorkflowSummary | null;
}
