(function () {
  const S = {
    pending: ['Queued', 'var(--queue)'], triaging: ['Triaging', 'var(--plan)'], planning: ['Planning', 'var(--plan)'],
    implementing: ['Implementing', 'var(--run)'], local_review_gate: ['Agent review', 'var(--rev)'],
    awaiting_feedback: ['Awaiting feedback', 'var(--rev)'], addressing_feedback: ['Addressing feedback', 'var(--run)'],
    ready_to_merge: ['Ready to merge', 'var(--ok)'], merging: ['Merging', 'var(--ok)'],
    blocked: ['Blocked', 'var(--warn)'], failed: ['Failed', 'var(--fail)'], done: ['Merged', 'var(--ok)'],
    cancelled: ['Cancelled', 'var(--queue)'],
  };
  const FLEET_ORDER = ['blocked', 'failed', 'implementing', 'addressing_feedback', 'local_review_gate', 'awaiting_feedback', 'ready_to_merge', 'merging', 'planning', 'triaging', 'pending'];
  const LIFE = ['triaging', 'planning', 'implementing', 'local_review_gate', 'awaiting_feedback', 'ready_to_merge', 'merging', 'done'];
  const LIFE_LABEL = ['Triage', 'Plan', 'Implement', 'Agent review', 'PR feedback', 'Ready', 'Merge', 'Done'];
  const lifeIdx = (state, from) => LIFE.indexOf(state === 'blocked' || state === 'failed' ? from : state);
  const K = {
    approval: { label: 'Approval requested', tone: 'var(--accent)', desc: 'Agent paused mid-turn and is waiting for a decision' },
    blocked: { label: 'Blocked', tone: 'var(--warn)', desc: 'Stopped by watchdog, planner or policy · unblock after resolving' },
    failed: { label: 'Failed', tone: 'var(--fail)', desc: 'Terminal failure · retry if classified retryable' },
    ready_to_merge: { label: 'Ready to merge', tone: 'var(--ok)', desc: 'Review and CI gates require operator attention' },
    awaiting_feedback: { label: 'Awaiting human feedback', tone: 'var(--rev)', desc: 'PR feedback needs attention' },
    driverless: { label: 'Driverless progress', tone: 'var(--warn)', desc: 'Workflow has no command driving it' },
  };
  const KIND_ORDER = ['approval', 'blocked', 'failed', 'driverless', 'ready_to_merge', 'awaiting_feedback'];
  window.HC = {
    S, FLEET_ORDER, LIFE, LIFE_LABEL, lifeIdx, K, KIND_ORDER,
    workflows: [], history: [], projects: [], hosts: [], breakers: [], failures: [], drafts: [], worktrees: [],
    health: { status: 'unknown', degraded: [], uptime: '—', logs: '—', logPath: '—', retention: '—' },
    retry: { at: '—', checked: 0, retried: 0, stuck: 0, skipped: 0 },
    rate: { signalSources: 0, signalLimit: 0, resetIds: 0, resetLimit: 0 },
    rules: { loaded: '—', failing: '—', lastCheck: '—' },
    maxSlots: 0, now: new Date().toLocaleString(), loading: true,
  };
})();
