import { useDashboard } from "@/lib/queries";
import { RuntimeCard } from "@/components/RuntimeCard";

interface Props {
  projectFilter?: string | null;
}

export function Channels({ projectFilter }: Props) {
  const { data } = useDashboard();
  const hosts = data?.runtime_hosts ?? [];

  return (
    <div>
      <h3 className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-3">Runtime Hosts</h3>
      {projectFilter && (
        <div className="mb-3 font-mono text-[11px] text-ink-4 border border-line px-3 py-2">
          per-project host filtering is unavailable — the backend exposes only a host count per project, not which projects each host watches. showing all hosts.
        </div>
      )}
      <div className="grid grid-cols-[repeat(auto-fill,minmax(300px,1fr))] gap-3">
        {hosts.map((h) => (
          <RuntimeCard
            key={h.id}
            runtime={{
              id: h.id,
              display_name: h.display_name,
              capabilities: h.capabilities,
              online: h.online,
              last_heartbeat_at: h.last_heartbeat_at,
              active_leases: h.active_leases,
              watched_projects: h.watched_projects,
              cpu_pct: null,
              ram_pct: null,
              tokens_24h: null,
            }}
          />
        ))}
      </div>
      {!hosts.length && (
        <div className="text-ink-4 font-mono text-[11px] p-5">no runtime hosts connected</div>
      )}
    </div>
  );
}
