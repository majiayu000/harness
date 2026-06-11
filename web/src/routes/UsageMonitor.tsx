import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { Panel } from "@/components/Panel";
import { KpiCard } from "@/components/KpiCard";
import { StatusBadge } from "@/components/StatusBadge";
import { PaletteFab } from "@/components/PaletteFab";
import { DOCS_URL } from "@/lib/links";
import { fmtInt } from "@/lib/format";
import { useUsageMonitor } from "@/lib/queries";
import type { ActiveCount, AgentInvocation, AgentProcess, LocalUsageSourceSummary, UsageGroup } from "@/types";

function fmtTokens(tokens: number | null | undefined): string {
  if (tokens == null || !Number.isFinite(tokens)) return "-";
  if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(tokens >= 10_000_000 ? 0 : 1)}M`;
  if (tokens >= 1_000) return `${(tokens / 1_000).toFixed(tokens >= 10_000 ? 0 : 1)}K`;
  return fmtInt(tokens);
}

function fmtCost(cost: number | null | undefined): string {
  if (cost == null || !Number.isFinite(cost)) return "-";
  if (cost >= 100) return `$${cost.toFixed(0)}`;
  if (cost >= 1) return `$${cost.toFixed(2)}`;
  return `$${cost.toFixed(4)}`;
}

function fmtDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}

function fmtBytes(bytes: number): string {
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(1)}GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(0)}MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)}KB`;
  return `${bytes}B`;
}

function burnClass(level: AgentInvocation["burn_level"]): string {
  switch (level) {
    case "high":
      return "text-danger border-danger/40";
    case "medium":
      return "text-warn border-warn/40";
    default:
      return "text-ink-3 border-line-2";
  }
}

function shortPath(path: string | null): string {
  if (!path) return "-";
  const parts = path.split("/");
  return parts.slice(-3).join("/");
}

function fmtRuntimeState(invocation: AgentInvocation): string {
  const leaseState = invocation.lease_state ? ` / ${invocation.lease_state.replaceAll("_", " ")}` : "";
  const inFlight = invocation.in_flight_model_turn ? " / in-flight" : "";
  return `${invocation.status}${leaseState}${inFlight} / ${invocation.workflow_state}`;
}

function fmtObservedAt(value: string | null): string {
  if (!value) return "-";
  return value.replace("T", " ").replace(/\.\d+Z$/, "Z");
}

function UsageGroupsTable({ rows, empty }: { rows: UsageGroup[]; empty: string }) {
  return (
    <div className="overflow-auto">
      <table className="w-full border-collapse font-mono text-[11.5px]">
        <thead className="text-ink-3 border-b border-line">
          <tr>
            <th className="text-left font-medium px-4 py-2">Name</th>
            <th className="text-right font-medium px-4 py-2">Tokens</th>
            <th className="text-right font-medium px-4 py-2">Requests</th>
            <th className="text-right font-medium px-4 py-2">Cost</th>
          </tr>
        </thead>
        <tbody>
          {rows.length ? (
            rows.slice(0, 12).map((row) => (
              <tr key={row.name} className="border-b border-line/70">
                <td className="px-4 py-2 text-ink-2 max-w-[220px] truncate" title={row.name}>{row.name}</td>
                <td className="px-4 py-2 text-right text-ink">{fmtTokens(row.total_tokens)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtInt(row.request_count)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtCost(row.estimated_cost_usd)}</td>
              </tr>
            ))
          ) : (
            <tr>
              <td className="px-4 py-6 text-ink-3" colSpan={4}>{empty}</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

function ActiveCountsList({ rows }: { rows: ActiveCount[] }) {
  return (
    <div className="p-4 space-y-2">
      {rows.length ? (
        rows.slice(0, 10).map((row) => (
          <div key={row.name} className="grid grid-cols-[1fr_48px_48px_48px_48px] gap-2 font-mono text-[11.5px]">
            <span className="truncate text-ink-2" title={row.name}>{row.name}</span>
            <span className="text-right text-ok">{fmtInt(row.active_leased)}</span>
            <span className="text-right text-sand">{fmtInt(row.pending)}</span>
            <span className="text-right text-warn">{fmtInt(row.expired_or_missing_lease)}</span>
            <span className="text-right text-danger">{fmtInt(row.high_burn)}</span>
          </div>
        ))
      ) : (
        <div className="font-mono text-[11.5px] text-ink-3">no active runtime jobs</div>
      )}
    </div>
  );
}

function AgentInvocationsTable({ invocations }: { invocations: AgentInvocation[] }) {
  return (
    <div className="overflow-auto">
      <table className="w-full border-collapse font-mono text-[11.5px]">
        <thead className="text-ink-3 border-b border-line">
          <tr>
            <th className="text-left font-medium px-4 py-2">Burn</th>
            <th className="text-left font-medium px-4 py-2">Invocation</th>
            <th className="text-left font-medium px-4 py-2">Repo</th>
            <th className="text-left font-medium px-4 py-2">Activity</th>
            <th className="text-left font-medium px-4 py-2">Runtime</th>
            <th className="text-left font-medium px-4 py-2">State</th>
            <th className="text-right font-medium px-4 py-2">Age</th>
            <th className="text-left font-medium px-4 py-2">Observed</th>
            <th className="text-left font-medium px-4 py-2">Owner</th>
          </tr>
        </thead>
        <tbody>
          {invocations.length ? (
            invocations.slice(0, 100).map((invocation) => (
              <tr key={invocation.agent_invocation_id} className="border-b border-line/70">
                <td className="px-4 py-2">
                  <span className={`px-1.5 py-[1px] border rounded-[10px] ${burnClass(invocation.burn_level)}`}>
                    {invocation.burn_level}
                  </span>
                </td>
                <td className="px-4 py-2 text-ink-2 max-w-[150px] truncate" title={invocation.agent_invocation_id}>
                  {invocation.agent_invocation_id}
                </td>
                <td className="px-4 py-2 text-ink-2 max-w-[180px] truncate" title={invocation.repo ?? invocation.project ?? ""}>
                  {invocation.repo ?? shortPath(invocation.project)}
                </td>
                <td className="px-4 py-2 text-ink max-w-[190px] truncate" title={invocation.activity}>{invocation.activity}</td>
                <td className="px-4 py-2 text-ink-2 max-w-[210px] truncate" title={invocation.model ?? invocation.runtime_profile}>
                  {invocation.agent_runtime}
                  {invocation.reasoning_effort ? <span className="text-ink-4"> / {invocation.reasoning_effort}</span> : null}
                </td>
                <td className="px-4 py-2 text-ink-2">{fmtRuntimeState(invocation)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtDuration(invocation.age_secs)}</td>
                <td className="px-4 py-2 text-ink-3 max-w-[180px] truncate" title={invocation.latest_runtime_event_type ?? ""}>
                  {fmtObservedAt(invocation.last_runtime_observation_at)}
                </td>
                <td className="px-4 py-2 text-ink-3 max-w-[160px] truncate" title={invocation.lease_owner ?? ""}>
                  {invocation.lease_owner ?? "-"}
                </td>
              </tr>
            ))
          ) : (
            <tr>
              <td className="px-4 py-6 text-ink-3" colSpan={9}>no active agent invocations</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

function ExternalProcessesTable({ processes }: { processes: AgentProcess[] }) {
  return (
    <div className="overflow-auto">
      <table className="w-full border-collapse font-mono text-[11.5px]">
        <thead className="text-ink-3 border-b border-line">
          <tr>
            <th className="text-left font-medium px-4 py-2">PID</th>
            <th className="text-left font-medium px-4 py-2">Agent</th>
            <th className="text-right font-medium px-4 py-2">Age</th>
            <th className="text-right font-medium px-4 py-2">CPU</th>
            <th className="text-right font-medium px-4 py-2">Memory</th>
            <th className="text-left font-medium px-4 py-2">Command</th>
          </tr>
        </thead>
        <tbody>
          {processes.length ? (
            processes.map((process) => (
              <tr key={process.pid} className="border-b border-line/70">
                <td className="px-4 py-2 text-ink">{process.pid}</td>
                <td className="px-4 py-2 text-ink-2">{process.agent}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtDuration(process.age_secs)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{process.cpu_pct.toFixed(1)}%</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtBytes(process.memory_bytes)}</td>
                <td className="px-4 py-2 text-ink-3 max-w-[220px] truncate">
                  {process.command_label || process.name}
                </td>
              </tr>
            ))
          ) : (
            <tr>
              <td className="px-4 py-6 text-ink-3" colSpan={6}>no external agent processes</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

function LocalUsageSourcesTable({ sources }: { sources: LocalUsageSourceSummary[] }) {
  return (
    <div className="overflow-auto">
      <table className="w-full border-collapse font-mono text-[11.5px]">
        <thead className="text-ink-3 border-b border-line">
          <tr>
            <th className="text-left font-medium px-4 py-2">Source</th>
            <th className="text-left font-medium px-4 py-2">Range</th>
            <th className="text-right font-medium px-4 py-2">Tokens</th>
            <th className="text-right font-medium px-4 py-2">Periods</th>
            <th className="text-right font-medium px-4 py-2">Cost</th>
            <th className="text-left font-medium px-4 py-2">Status</th>
          </tr>
        </thead>
        <tbody>
          {sources.length ? (
            sources.map((source) => (
              <tr key={source.source} className="border-b border-line/70">
                <td className="px-4 py-2 text-ink">{source.display_name}</td>
                <td className="px-4 py-2 text-ink-3">{source.since} / {source.until}</td>
                <td className="px-4 py-2 text-right text-ink">{fmtTokens(source.total_tokens)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtInt(source.period_count)}</td>
                <td className="px-4 py-2 text-right text-ink-2">{fmtCost(source.estimated_cost_usd)}</td>
                <td className="px-4 py-2 text-ink-3 max-w-[260px] truncate" title={source.error ?? source.cost_confidence}>
                  {source.status}
                  {source.model_count ? <span className="text-ink-4"> / {fmtInt(source.model_count)} models</span> : null}
                </td>
              </tr>
            ))
          ) : (
            <tr>
              <td className="px-4 py-6 text-ink-3" colSpan={6}>no local source summaries</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

export function UsageMonitor() {
  const { data, isError } = useUsageMonitor();

  const sections: SidebarSection[] = [
    {
      label: "System",
      items: [
        { id: "overview", label: "Overview", href: "/overview" },
        { id: "usage", label: "Usage", href: "/usage", active: true },
        { id: "projects", label: "Projects", href: "/overview#projects" },
        { id: "runtimes", label: "Runtimes", href: "/overview#runtimes" },
      ],
    },
    {
      label: "Fleet",
      items: [
        { id: "tasks", label: "All tasks", href: "/overview#projects" },
        { id: "worktrees", label: "Worktrees", href: "/worktrees" },
      ],
    },
    {
      label: "Reference",
      items: [{ id: "docs", label: "Docs", href: DOCS_URL }],
    },
  ];

  return (
    <div className="grid grid-cols-[240px_1fr] h-screen overflow-hidden">
      <Sidebar env="local" sections={sections} />
      <main className="flex flex-col min-h-0 min-w-0">
        <TopBar
          breadcrumb={[{ label: "system" }, { label: "usage", current: true }]}
          searchPlaceholder="Search usage, jobs, processes..."
          actions={<StatusBadge ok={!isError} />}
        />
        <div className="flex-1 overflow-auto min-h-0">
          <div className="px-6 py-4.5 border-b border-line bg-bg flex items-center gap-4">
            <h1 className="m-0 text-xl font-medium tracking-[-0.01em]">
              Usage <em className="font-serif italic text-rust font-normal">monitor</em>
            </h1>
            <span className="font-mono text-[12px] text-ink-3 ml-1">
              {data ? `${data.window.hours}h window / ${fmtInt(data.agent_invocations.length)} invocations` : "loading..."}
            </span>
          </div>

          <div className="grid grid-cols-6 border-b border-line">
            <KpiCard label="Tokens" value={fmtTokens(data?.summary.total_tokens)} delta={`${fmtInt(data?.summary.request_count)} requests`} />
            <KpiCard
              label="Est. cost"
              value={fmtCost(data?.summary.estimated_cost_usd)}
              delta={data?.cost.configured ? `${data.cost.currency} catalog` : "price unavailable"}
            />
            <KpiCard
              label="Active leases"
              value={fmtInt(data?.summary.active_leased_agent_invocations)}
              delta={`${fmtInt(data?.summary.expired_or_missing_lease_agent_invocations)} expired/missing, ${fmtInt(data?.summary.pending_agent_invocations)} pending`}
            />
            <KpiCard label="High burn" value={fmtInt(data?.summary.high_burn_invocations)} delta={`${fmtInt(data?.summary.stale_agent_invocations)} stale`} />
            <KpiCard label="External procs" value={fmtInt(data?.summary.external_agent_processes)} delta="local CLI only" />
            <KpiCard label="Models" value={fmtInt(data?.tokens_by_model.length)} delta={`${fmtInt(data?.cost.missing_model_count)} unpriced`} />
          </div>

          <div className="grid grid-cols-[1.2fr_1fr]">
            <Panel title="Agent invocations" sub={`${fmtInt(data?.agent_invocations.length)} recent or active`} className="border-r border-line">
              <AgentInvocationsTable invocations={data?.agent_invocations ?? []} />
            </Panel>
            <Panel
              title="Active pressure"
              sub="active / pending / expired / high"
            >
              <div className="grid grid-cols-2">
                <Panel title="By repo" className="border-r border-line">
                  <ActiveCountsList rows={data?.active_by_repo ?? []} />
                </Panel>
                <Panel title="By activity">
                  <ActiveCountsList rows={data?.active_by_activity ?? []} />
                </Panel>
              </div>
            </Panel>
          </div>

          <div className="grid grid-cols-3">
            <Panel title="Tokens by agent" sub="completed turns" className="border-r border-line">
              <UsageGroupsTable rows={data?.tokens_by_agent ?? []} empty="no token events" />
            </Panel>
            <Panel title="Tokens by project" sub="completed turns" className="border-r border-line">
              <UsageGroupsTable rows={data?.tokens_by_project ?? []} empty="no project usage" />
            </Panel>
            <Panel title="Tokens by model" sub="completed turns">
              <UsageGroupsTable rows={data?.tokens_by_model ?? []} empty="no model usage" />
            </Panel>
          </div>

          <Panel
            title="Local ccstats sources"
            sub="global Codex and Claude logs outside workflow attribution"
          >
            <LocalUsageSourcesTable sources={data?.local_usage_sources ?? []} />
          </Panel>

          <Panel
            title="External local CLI processes"
            sub={`${fmtInt(data?.external_agent_processes.length)} codex / claude processes outside runtime attribution`}
          >
            <ExternalProcessesTable processes={data?.external_agent_processes ?? []} />
          </Panel>

          <Panel
            title="Cost source"
            sub={data?.cost.configured ? data.cost.source : "not configured"}
          >
            <div className="p-4 font-mono text-[11.5px] text-ink-3">
              {data?.cost.message ?? "loading..."}
            </div>
          </Panel>
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
