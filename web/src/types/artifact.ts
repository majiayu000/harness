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
