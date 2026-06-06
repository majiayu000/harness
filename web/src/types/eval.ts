export interface EvalQualitySnapshotsResponse {
  quality_snapshots: QualitySnapshotRecord[];
}

export interface QualitySnapshotRecord {
  id: string;
  run_id: string;
  snapshot: QualitySnapshot;
  created_at: string;
}

export interface QualitySnapshot {
  scenario: string;
  run_mode: string;
  target: EvalTarget;
  baseline_pr: PullRequestSnapshot | null;
  final_pr: PullRequestSnapshot | null;
  runtime: RuntimeSnapshot | null;
  hard_gates: HardGateResult[];
  final_score: number;
  final_grade: string;
  grade_cap: string | null;
  blocker_summary: string[];
}

export type EvalTarget =
  | {
      kind: "pull_request";
      repo: string;
      pr_number: number;
      base_ref: string | null;
      head_ref: string | null;
    }
  | { kind: "issue"; repo: string; issue_number: number }
  | { kind: "prompt_task"; task_id: string };

export interface PullRequestSnapshot {
  repo: string;
  pr_number: number;
  url: string | null;
  title: string | null;
  base_ref: string;
  head_ref: string;
  head_oid: string;
  is_draft: boolean;
  merge_state: string;
  check_state: string;
  review_decision: string | null;
  active_unresolved_review_threads: ReviewThreadSnapshot[];
  review_threads_complete: boolean;
  changed_files: ChangedFileSnapshot[];
  changed_files_complete: boolean;
  collected_at: string;
}

export interface ReviewThreadSnapshot {
  id: string;
  path: string | null;
  is_resolved: boolean;
  is_outdated: boolean;
}

export interface ChangedFileSnapshot {
  path: string;
  additions: number;
  deletions: number;
  status: string;
}

export interface RuntimeSnapshot {
  task_id: string | null;
  workflow_id: string | null;
  workflow_state: string | null;
  latest_activity: string | null;
  terminal_state: string | null;
  collected_at: string;
}

export interface HardGateResult {
  name: string;
  status: string;
  grade_cap: string | null;
  message: string;
}
