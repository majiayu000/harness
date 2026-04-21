export interface RetryTickSummary {
  checked: number;
  retried: number;
  stuck: number;
  skipped: number;
  at: string;
}

export interface StalledTask {
  task_id: string;
  external_id: string;
  project: string;
  status: string;
  stalled_since: string;
}

export interface RecentFailure {
  task_id: string;
  external_id: string;
  project: string;
  error: string;
  failed_at: string;
}

export interface SignalIngestionLimits {
  tracked_sources: number;
  limit_per_minute: number;
}

export interface PasswordResetLimits {
  tracked_identifiers: number;
  limit_per_hour: number;
}

export interface OperatorSnapshotPayload {
  generated_at: string;
  retry: {
    last_tick: RetryTickSummary | null;
    stalled_tasks: StalledTask[];
  };
  rate_limits: {
    signal_ingestion: SignalIngestionLimits;
    password_reset: PasswordResetLimits;
  };
  recent_failures: RecentFailure[];
}
