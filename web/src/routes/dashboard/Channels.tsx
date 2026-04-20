import { useDashboard } from "@/lib/queries";
import { RuntimeCard } from "@/components/RuntimeCard";

interface Props {
  projectFilter?: string | null;
}

export function Channels({ projectFilter }: Props) {
  const { data } = useDashboard();
  const allHosts = data?.runtime_hosts ?? [];

  const resolvedRoot = projectFilter
    ? (data?.projects.find((p) => p.id === projectFilter)?.root ?? projectFilter)
    : null;

  const hosts = resolvedRoot
    ? allHosts.filter((h) => h.watched_project_roots.includes(resolvedRoot))
    : allHosts;

  return (
    <div>
      <h3 className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-3">Runtime Hosts</h3>
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
