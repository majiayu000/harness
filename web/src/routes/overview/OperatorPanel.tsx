import { Panel } from "@/components/Panel";
import { useOperatorSnapshot } from "@/lib/queries";
import type { RecentFailure, StalledTask } from "@/types";

function timeAgo(iso: string): string {
  const secs = Math.max(0, Math.floor((Date.now() - new Date(iso).getTime()) / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

function Counter({ label, value }: { label: string; value: number }) {
  return (
    <div className="flex flex-col items-center gap-0.5 px-4 py-2 border-r border-line last:border-r-0">
      <span className="font-mono text-lg font-semibold text-ink-1">{value}</span>
      <span className="font-mono text-[10px] text-ink-4 uppercase tracking-wider">{label}</span>
    </div>
  );
}

function StalledRow({ task }: { task: StalledTask }) {
  return (
    <div className="flex items-center gap-3 px-4 py-1.5 border-b border-line last:border-b-0 font-mono text-[11px]">
      <span className="text-ink-2 truncate flex-1">{task.external_id}</span>
      <span className="text-ink-4">{task.project}</span>
      <span className="text-ink-4">{timeAgo(task.stalled_since)}</span>
      <span className="text-ink-3">{task.status}</span>
    </div>
  );
}

function FailureRow({ failure }: { failure: RecentFailure }) {
  return (
    <div className="flex items-center gap-3 px-4 py-1.5 border-b border-line last:border-b-0 font-mono text-[11px]">
      <span className="text-ink-2 truncate w-32">{failure.external_id}</span>
      <span className="text-rust truncate flex-1">{failure.error}</span>
      <span className="text-ink-4 shrink-0">{timeAgo(failure.failed_at)}</span>
    </div>
  );
}

export function OperatorPanel() {
  const { data, isError } = useOperatorSnapshot();

  const tick = data?.retry.last_tick ?? null;
  const stalled = data?.retry.stalled_tasks ?? [];
  const sig = data?.rate_limits.signal_ingestion;
  const pw = data?.rate_limits.password_reset;
  const failures = data?.recent_failures ?? [];

  return (
    <Panel title="Operator snapshot" sub="retry · rate limits · recent failures" id="operator">
      {/* Retry pressure */}
      <div className="border-b border-line">
        <div className="px-4 py-2 font-mono text-[10px] text-ink-3 uppercase tracking-wider border-b border-line">
          Retry pressure
        </div>
        {isError ? (
          <p className="px-4 py-3 font-mono text-[11px] text-danger">Operator snapshot unavailable.</p>
        ) : tick ? (
          <div className="flex">
            <Counter label="checked" value={tick.checked} />
            <Counter label="retried" value={tick.retried} />
            <Counter label="stuck" value={tick.stuck} />
            <Counter label="skipped" value={tick.skipped} />
          </div>
        ) : (
          <p className="px-4 py-3 font-mono text-[11px] text-ink-4">No retry ticks recorded yet.</p>
        )}
        {stalled.length > 0 && (
          <div className="border-t border-line">
            {stalled.map((t) => (
              <StalledRow key={t.task_id} task={t} />
            ))}
          </div>
        )}
      </div>

      {/* Rate limits */}
      <div className="border-b border-line">
        <div className="px-4 py-2 font-mono text-[10px] text-ink-3 uppercase tracking-wider border-b border-line">
          Rate limits
        </div>
        <table className="w-full font-mono text-[11px]">
          <tbody>
            <tr className={`border-b border-line ${(sig?.tracked_sources ?? 0) > 0 ? "bg-sand/10" : ""}`}>
              <td className="px-4 py-1.5 text-ink-2">Signal ingestion</td>
              <td className="px-4 py-1.5 text-ink-1 text-right">{sig?.tracked_sources ?? 0}</td>
              <td className="px-4 py-1.5 text-ink-4 text-right">/ {sig?.limit_per_minute ?? 0} /min</td>
            </tr>
            <tr className={(pw?.tracked_identifiers ?? 0) > 0 ? "bg-sand/10" : ""}>
              <td className="px-4 py-1.5 text-ink-2">Password reset</td>
              <td className="px-4 py-1.5 text-ink-1 text-right">{pw?.tracked_identifiers ?? 0}</td>
              <td className="px-4 py-1.5 text-ink-4 text-right">/ {pw?.limit_per_hour ?? 0} /hr</td>
            </tr>
          </tbody>
        </table>
      </div>

      {/* Recent failures */}
      <div>
        <div className="px-4 py-2 font-mono text-[10px] text-ink-3 uppercase tracking-wider border-b border-line">
          Recent failures
        </div>
        {isError ? (
          <p className="px-4 py-3 font-mono text-[11px] text-danger">Operator snapshot unavailable.</p>
        ) : failures.length === 0 ? (
          <p className="px-4 py-3 font-mono text-[11px] text-ink-4">No recent failures.</p>
        ) : (
          failures.map((f) => <FailureRow key={f.task_id} failure={f} />)
        )}
      </div>
    </Panel>
  );
}
