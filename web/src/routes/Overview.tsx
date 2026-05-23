import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { Panel } from "@/components/Panel";
import { KpiCard } from "@/components/KpiCard";
import { StackedArea, type StackedAreaSeries } from "@/components/StackedArea";
import { HeatmapRow } from "@/components/HeatmapRow";
import { ProjectsTable } from "@/components/ProjectsTable";
import { RuntimeCard } from "@/components/RuntimeCard";
import { Feed } from "@/components/Feed";
import { AlertList } from "@/components/AlertList";
import { StatusBadge } from "@/components/StatusBadge";
import { PaletteFab } from "@/components/PaletteFab";
import { EvolutionCard } from "@/components/EvolutionCard";
import { useOperatorSnapshot, useOverview } from "@/lib/queries";
import { DOCS_URL } from "@/lib/links";
import { OperatorPanel } from "./overview/OperatorPanel";
import { fmtInt, fmtPct, fmtScore } from "@/lib/format";

const SERIES_COLORS = [
  "var(--rust)",
  "var(--moss)",
  "var(--sky)",
  "var(--plum)",
  "var(--sand)",
  "var(--rust-soft)",
];

function fmtTokenCompact(tokens: number): string {
  if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(tokens >= 10_000_000 ? 0 : 1)}M`;
  if (tokens >= 1_000) return `${(tokens / 1_000).toFixed(tokens >= 10_000 ? 0 : 1)}K`;
  return fmtInt(tokens);
}

export function Overview() {
  const { data, isError } = useOverview();
  const { isError: isOperatorSnapshotError } = useOperatorSnapshot();
  const isSystemHealthy = !isError && !isOperatorSnapshotError;
  const agentTokens = [...(data?.agent_tokens ?? [])].sort((a, b) => b.tokens_24h - a.tokens_24h);
  const maxAgentTokens = Math.max(...agentTokens.map((a) => a.tokens_24h), 0);

  const sections: SidebarSection[] = [
    {
      label: "System",
      items: [
        { id: "overview", label: "Overview", href: "/overview", active: true },
        { id: "projects", label: "Projects", href: "/overview#projects", count: data?.projects.length },
        { id: "runtimes", label: "Runtimes", href: "/overview#runtimes", count: data?.runtimes.length },
        { id: "observability", label: "Observability", href: "/overview#observability" },
      ],
    },
    {
      label: "Fleet",
      items: [
        { id: "tasks", label: "All tasks", href: "/overview#projects", count: data?.kpi.active_tasks },
        { id: "worktrees", label: "Worktrees", href: "/worktrees", count: data?.kpi.worktrees.used },
      ],
    },
    {
      label: "Reference",
      items: [{ id: "docs", label: "Docs", href: DOCS_URL }],
    },
  ];

  const throughputSeries: StackedAreaSeries[] = (data?.throughput.series ?? []).map((s, i) => ({
    name: s.project,
    values: s.values,
    color: SERIES_COLORS[i % SERIES_COLORS.length],
  }));

  return (
    <div className="grid grid-cols-[240px_1fr] h-screen overflow-hidden">
      <Sidebar env="local" sections={sections} />
      <main className="flex flex-col min-h-0 min-w-0">
        <TopBar
          breadcrumb={[{ label: "system" }, { label: "overview", current: true }]}
          searchPlaceholder="Search projects, runtimes, tasks…"
        />
        <div className="flex-1 overflow-auto min-h-0">
          <div className="px-6 py-4.5 border-b border-line bg-bg flex items-center gap-4">
            <h1 className="m-0 text-xl font-medium tracking-[-0.01em]">
              System <em className="font-serif italic text-rust font-normal">overview</em>
            </h1>
            <span className="font-mono text-[12px] text-ink-3 ml-1">
              {data ? `${data.projects.length} projects · ${data.runtimes.length} runtimes` : "loading…"}
            </span>
            <div className="ml-auto flex gap-2 items-center">
              <StatusBadge ok={isSystemHealthy} />
            </div>
          </div>

          <div className="grid grid-cols-6 border-b border-line">
            <KpiCard label="Active tasks" value={fmtInt(data?.kpi.active_tasks)} delta={`window ${data?.window.hours ?? 24}h`} />
            <KpiCard label="Merged · 24h" value={fmtInt(data?.kpi.merged_24h)} delta="in window" />
            <KpiCard
              label="Avg review score"
              value={fmtScore(data?.kpi.avg_review_score ?? null)}
              unit="/100"
              delta={data?.kpi.grade ? `grade ${data.kpi.grade}` : "no scans"}
            />
            <KpiCard label="Rule fail rate" value={fmtPct(data?.kpi.rule_fail_rate_pct ?? 0)} unit="%" delta="rule_check" />
            <KpiCard
              label="Tokens · 24h"
              value={data?.kpi.tokens_24h != null ? fmtInt(data.kpi.tokens_24h) : "—"}
              delta={agentTokens.length ? `${agentTokens.length} agents` : "no usage"}
            />
            <KpiCard
              label="Worktrees"
              value={`${fmtInt(data?.kpi.worktrees.used)}`}
              unit={`/${fmtInt(data?.kpi.worktrees.total)}`}
              delta={`${
                data?.kpi.worktrees.total
                  ? Math.round(((data.kpi.worktrees.used ?? 0) / data.kpi.worktrees.total) * 100)
                  : 0
              }% util`}
            />
          </div>

          <div className="grid grid-cols-[1.6fr_1fr]">
            <Panel title="Fleet throughput" sub="tasks per hour, stacked by project" className="border-r border-line">
              <div className="px-5 py-4 h-[240px]">
                <StackedArea series={throughputSeries} />
              </div>
            </Panel>
            <Panel
              title="Task distribution"
              sub={`${fmtInt(Object.values(data?.distribution ?? {}).reduce((a, b) => a + b, 0))} tasks`}
            >
              <div className="p-5">
                <div className="font-mono text-[11px] text-ink-3">
                  queued {fmtInt(data?.distribution.queued)} · running {fmtInt(data?.distribution.running)} · review{" "}
                  {fmtInt(data?.distribution.review)} · merged {fmtInt(data?.distribution.merged)} · failed{" "}
                  {fmtInt(data?.distribution.failed)}
                </div>
                <div className="mt-5 border-t border-line pt-4">
                  <div className="font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Agent usage</div>
                  <div className="mt-3 space-y-2">
                    {agentTokens.length ? (
                      agentTokens.map((item) => {
                        const width = maxAgentTokens > 0 ? Math.max(4, (item.tokens_24h / maxAgentTokens) * 100) : 0;
                        return (
                          <div key={item.agent} className="grid grid-cols-[72px_1fr_56px] items-center gap-3">
                            <span className="font-mono text-[11px] text-ink-2 truncate">{item.agent}</span>
                            <span className="h-2 bg-bg-2 border border-line">
                              <span
                                className="block h-full bg-rust"
                                style={{ width: `${width}%` }}
                              />
                            </span>
                            <span className="font-mono text-[11px] text-right text-ink-2">
                              {fmtTokenCompact(item.tokens_24h)}
                            </span>
                          </div>
                        );
                      })
                    ) : (
                      <div className="font-mono text-[11px] text-ink-3">no token events</div>
                    )}
                  </div>
                </div>
              </div>
            </Panel>
          </div>

          <Panel title="Projects" sub="click a row to open the dashboard" id="projects">
            <ProjectsTable projects={data?.projects ?? []} />
          </Panel>

          <Panel title="Fleet runtimes" sub="connected machines and cloud endpoints" id="runtimes">
            <div className="px-5 py-3.5 grid grid-cols-[repeat(auto-fill,minmax(300px,1fr))] gap-3">
              {(data?.runtimes ?? []).map((r) => (
                <RuntimeCard key={r.id} runtime={r} />
              ))}
            </div>
          </Panel>

          <div id="observability" className="grid grid-cols-[1.6fr_1fr] scroll-mt-12">
            <Panel title="Cluster activity" sub="hourly buckets" className="border-r border-line">
              <div className="p-5">
                {(data?.heatmap.rows ?? []).map((r) => (
                  <HeatmapRow key={r.label} label={r.label} intensity={r.intensity} />
                ))}
              </div>
            </Panel>
            <Panel title="Live activity" sub="cross-project stream">
              <Feed entries={data?.feed ?? []} />
              <Panel title="System alerts" sub={`${data?.alerts.length ?? 0} open`} className="border-t border-line">
                <AlertList alerts={data?.alerts ?? []} />
              </Panel>
            </Panel>
          </div>

          <EvolutionCard evolution={data?.evolution ?? null} />

          <OperatorPanel />
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
