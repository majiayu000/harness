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

export interface TaskArtifact {
  task_id: string;
  turn: number;
  artifact_type: string;
  content: string;
  created_at: string;
}

export interface TaskPrompt {
  task_id: string;
  turn: number;
  phase: string;
  prompt: string;
  created_at: string;
}

export function isTerminal(status: string): boolean {
  return status === "done" || status === "cancelled" || status === "failed";
}

export interface WorkflowSummary {
  state: string;
  issue_number?: number | null;
  pr_number?: number | null;
  force_execute?: boolean;
  plan_concern?: string | null;
}

export interface RoundItem {
  kind: string;
  content?: string | null;
}

export interface TaskRound {
  items: RoundItem[];
}

export interface FullTask extends Task {
  rounds: TaskRound[];
  system_input?: string | null;
  request_settings?: string | null;
}

export type StreamItem =
  | { type: "MessageDelta"; text: string }
  | { type: "Done" }
  | { type: "Error"; message: string }
  | { type: "TokenUsage"; usage: { input_tokens: number; output_tokens: number } };

export type CreateTaskPayload =
  | { issue: number; project?: string; agent?: string; max_turns?: number; turn_timeout_secs?: number }
  | { pr: number; project?: string; agent?: string; max_turns?: number; turn_timeout_secs?: number }
  | { prompt: string; project?: string; agent?: string; max_turns?: number; turn_timeout_secs?: number };

export interface CreateTaskResponse {
  task_id: string;
  status: string;
}
