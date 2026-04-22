const EM_DASH = "—";

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

export function formatDurationShort(seconds: number | null | undefined): string {
  if (!isFiniteNumber(seconds)) return EM_DASH;
  if (seconds < 60) return `${Math.floor(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

export function relativeAgo(then: Date | string, now: Date = new Date()): string {
  const thenDate = typeof then === "string" ? new Date(then) : then;
  const seconds = Math.max(0, Math.floor((now.getTime() - thenDate.getTime()) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}
