const EM_DASH = "—";

export function workflowLabel(state: string): string {
  switch (state) {
    case "discovered": return "Discovered";
    case "scheduled": return "Scheduled";
    case "implementing": return "Implementing";
    case "pr_open": return "PR Open";
    case "awaiting_feedback": return "Awaiting Feedback";
    case "addressing_feedback": return "Addressing Feedback";
    case "ready_to_merge": return "Ready To Merge";
    case "blocked": return "Blocked";
    case "done": return "Done";
    case "failed": return "Failed";
    case "cancelled": return "Cancelled";
    case "idle": return "Idle";
    case "polling_intake": return "Polling Intake";
    case "planning_batch": return "Planning Batch";
    case "dispatching": return "Dispatching";
    case "monitoring": return "Monitoring";
    case "sweeping_feedback": return "Sweeping Feedback";
    case "paused": return "Paused";
    case "degraded": return "Degraded";
    default: return state;
  }
}

function isFiniteNumber(n: unknown): n is number {
  return typeof n === "number" && Number.isFinite(n);
}

export function fmtInt(n: number | null | undefined): string {
  if (!isFiniteNumber(n)) return EM_DASH;
  return n.toLocaleString("en-US");
}

export function fmtScore(n: number | null | undefined): string {
  if (!isFiniteNumber(n)) return EM_DASH;
  return Math.round(n).toString();
}

export function fmtPct(n: number | null | undefined): string {
  if (!isFiniteNumber(n)) return EM_DASH;
  return n.toFixed(1);
}

export function relativeAgo(then: Date | string, now: Date = new Date()): string {
  const thenDate = typeof then === "string" ? new Date(then) : then;
  const seconds = Math.max(0, Math.floor((now.getTime() - thenDate.getTime()) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}
