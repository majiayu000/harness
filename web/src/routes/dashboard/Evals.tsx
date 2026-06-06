import { useMemo, useState } from "react";
import { fmtInt, formatSnakeLabel, relativeAgo, shortSha } from "@/lib/format";
import { useEvalDashboard } from "@/lib/queries";
import type {
  EvalDashboardRow,
  EvalRun,
  EvalTarget,
  HardGateResult,
  QualitySnapshot,
  UsageSnapshot,
} from "@/types";

type Filter = "all" | "live" | "collect" | "blocked";

function targetLabel(target: EvalTarget): string {
  switch (target.kind) {
    case "pull_request":
      return `${target.repo}#${target.pr_number}`;
    case "issue":
      return `${target.repo}#${target.issue_number}`;
    case "prompt_task":
      return target.task_id;
  }
}

function targetHref(target: EvalTarget, snapshot: QualitySnapshot | null): string | null {
  if (target.kind !== "pull_request") return null;
  return snapshot?.final_pr?.url ?? snapshot?.baseline_pr?.url ?? null;
}

function failedGates(snapshot: QualitySnapshot | null): HardGateResult[] {
  return snapshot?.hard_gates.filter((gate) => gate.status === "fail") ?? [];
}

function gateLabel(snapshot: QualitySnapshot | null): string {
  if (!snapshot) return "N/A";
  const passed = snapshot.hard_gates.filter((gate) => gate.status === "pass").length;
  return `${passed}/${snapshot.hard_gates.length}`;
}

function blockerCount(row: EvalDashboardRow): number {
  const snapshot = row.quality_snapshot?.snapshot ?? null;
  return failedGates(snapshot).length + (snapshot?.blocker_summary.length ?? 0);
}

function rowRunMode(row: EvalDashboardRow): string | null {
  return row.quality_snapshot?.snapshot.run_mode ?? null;
}

function rowMatchesFilter(row: EvalDashboardRow, filter: Filter): boolean {
  if (filter === "all") return true;
  if (filter === "blocked") return blockerCount(row) > 0 || !!row.quality_snapshot_error;
  const mode = rowRunMode(row);
  if (filter === "live") return mode === "live_run";
  if (filter === "collect") return mode === "collect_only";
  return true;
}

function rowMatchesQuery(row: EvalDashboardRow, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const snapshot = row.quality_snapshot?.snapshot;
  return [
    row.run.id,
    row.run.scenario,
    row.run.status,
    row.run.source_task_id,
    targetLabel(row.run.target),
    snapshot?.final_grade,
    snapshot?.final_pr?.head_oid,
  ].some((value) => value?.toLowerCase().includes(q));
}

function gradeClass(grade: string | null | undefined): string {
  if (grade === "A") return "border-ok/40 bg-ok/10 text-ok";
  if (grade === "B") return "border-sky/40 bg-sky/10 text-sky";
  if (grade === "C") return "border-warn/40 bg-warn/10 text-warn";
  if (grade === "D" || grade === "F") return "border-rust/40 bg-rust/10 text-rust";
  return "border-line bg-bg text-ink-3";
}

function evalStatusClass(run: EvalRun, blocked: boolean): string {
  if (blocked) return "border-rust/40 bg-rust/10 text-rust";
  if (run.status === "scored") return "border-ok/40 bg-ok/10 text-ok";
  return "border-line bg-bg text-ink-3";
}

function usageTotals(usage: UsageSnapshot[] | undefined): { tokens: number | null; micros: number | null } {
  if (!usage || usage.length === 0) return { tokens: null, micros: null };
  let hasTokens = false;
  let tokens = 0;
  let hasCost = false;
  let micros = 0;
  for (const item of usage) {
    if (typeof item.total_tokens === "number") {
      hasTokens = true;
      tokens += item.total_tokens;
    }
    if (typeof item.cost_usd_micros === "number") {
      hasCost = true;
      micros += item.cost_usd_micros;
    }
  }
  return { tokens: hasTokens ? tokens : null, micros: hasCost ? micros : null };
}

function formatCost(micros: number | null): string {
  if (micros === null) return "N/A";
  return `$${(micros / 1_000_000).toFixed(4)}`;
}

function sortRows(rows: EvalDashboardRow[]): EvalDashboardRow[] {
  return [...rows].sort((a, b) => {
    const aTime = Date.parse(a.run.updated_at);
    const bTime = Date.parse(b.run.updated_at);
    return (Number.isNaN(bTime) ? 0 : bTime) - (Number.isNaN(aTime) ? 0 : aTime);
  });
}

export function Evals() {
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");
  const { data, isLoading, isError } = useEvalDashboard(50);

  const rows = useMemo(() => {
    return sortRows(data?.rows ?? []).filter(
      (row) => rowMatchesFilter(row, filter) && rowMatchesQuery(row, query),
    );
  }, [data?.rows, filter, query]);

  const scored = (data?.rows ?? []).filter((row) => row.run.status === "scored").length;
  const blocked = (data?.rows ?? []).filter((row) => blockerCount(row) > 0 || row.quality_snapshot_error).length;
  const live = (data?.rows ?? []).filter((row) => rowRunMode(row) === "live_run").length;
  const visibleSnapshots = rows
    .map((row) => row.quality_snapshot?.snapshot)
    .filter((snapshot): snapshot is QualitySnapshot => !!snapshot);
  const averageScore =
    visibleSnapshots.length === 0
      ? null
      : Math.round(
          visibleSnapshots.reduce((sum, snapshot) => sum + snapshot.final_score, 0) /
            visibleSnapshots.length,
        );

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-4 gap-3">
        <Metric label="Runs" value={fmtInt(data?.rows.length ?? 0)} />
        <Metric label="Scored" value={fmtInt(scored)} />
        <Metric label="Live" value={fmtInt(live)} />
        <Metric label="Blocked" value={fmtInt(blocked)} tone={blocked > 0 ? "bad" : "ok"} />
      </div>

      <div className="flex gap-2">
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search evals…"
          className="flex-1 h-[30px] bg-bg-1 border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
        <select
          value={filter}
          onChange={(event) => setFilter(event.target.value as Filter)}
          className="h-[30px] bg-bg-1 border border-line-2 text-ink font-mono text-[12px] px-2 rounded-[3px]"
        >
          <option value="all">All</option>
          <option value="live">Live</option>
          <option value="collect">Collect-only</option>
          <option value="blocked">Blocked</option>
        </select>
      </div>

      <div className="border border-line bg-bg-1">
        <div className="flex items-center justify-between border-b border-line px-3 py-2 font-mono text-[11px] text-ink-3">
          <span>{rows.length} eval runs</span>
          <span>avg score {averageScore === null ? "N/A" : `${averageScore}/100`}</span>
        </div>

        {isLoading ? (
          <div className="px-3 py-8 text-center font-mono text-[12px] text-ink-3">Loading evals…</div>
        ) : isError ? (
          <div className="px-3 py-8 text-center font-mono text-[12px] text-rust">Evals unavailable</div>
        ) : rows.length === 0 ? (
          <div className="px-3 py-10 text-center font-mono text-[12px] text-ink-3">No eval runs found.</div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[980px] border-collapse text-left">
              <thead className="border-b border-line bg-bg">
                <tr className="font-mono text-[10px] uppercase tracking-[0.08em] text-ink-4">
                  <th className="w-[90px] px-3 py-2 font-normal">Updated</th>
                  <th className="px-3 py-2 font-normal">Target</th>
                  <th className="w-[120px] px-3 py-2 font-normal">Scenario</th>
                  <th className="w-[120px] px-3 py-2 font-normal">Status</th>
                  <th className="w-[110px] px-3 py-2 font-normal">Score</th>
                  <th className="w-[90px] px-3 py-2 font-normal">Gates</th>
                  <th className="w-[130px] px-3 py-2 font-normal">Usage</th>
                  <th className="w-[220px] px-3 py-2 font-normal">Blocker</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <EvalRow key={row.run.id} row={row} />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: "ok" | "bad" }) {
  const toneClass = tone === "bad" ? "text-rust" : tone === "ok" ? "text-ok" : "text-ink";
  return (
    <div className="border border-line bg-bg-1 px-3 py-2">
      <div className="font-mono text-[10px] uppercase tracking-[0.08em] text-ink-4">{label}</div>
      <div className={`mt-1 font-mono text-[20px] leading-none ${toneClass}`}>{value}</div>
    </div>
  );
}

function EvalRow({ row }: { row: EvalDashboardRow }) {
  const snapshot = row.quality_snapshot?.snapshot ?? null;
  const target = targetLabel(row.run.target);
  const href = targetHref(row.run.target, snapshot);
  const blockers = [
    ...(snapshot?.blocker_summary ?? []),
        ...failedGates(snapshot).map((gate) => `${formatSnakeLabel(gate.name)}: ${gate.message}`),
  ];
  const totals = usageTotals(snapshot?.usage);
  const blocked = blockers.length > 0 || !!row.quality_snapshot_error;

  return (
    <tr className="border-b border-line last:border-b-0">
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
        {row.run.updated_at ? relativeAgo(row.run.updated_at) : "N/A"}
      </td>
      <td className="px-3 py-2">
        <div className="min-w-0">
          {href ? (
            <a href={href} target="_blank" rel="noreferrer" className="font-mono text-[11px] text-rust hover:underline">
              {target}
            </a>
          ) : (
            <span className="font-mono text-[11px] text-ink">{target}</span>
          )}
          <div className="mt-1 truncate font-mono text-[10px] text-ink-4" title={row.run.id}>
            {row.run.id}
          </div>
          {snapshot?.final_pr?.head_oid ? (
            <div className="mt-1 font-mono text-[10px] text-ink-3">
              head {shortSha(snapshot.final_pr.head_oid)}
            </div>
          ) : null}
        </div>
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
        <div>{formatSnakeLabel(row.run.scenario)}</div>
        <div className="mt-1 text-[10px] text-ink-4">{formatSnakeLabel(snapshot?.run_mode)}</div>
      </td>
      <td className="px-3 py-2">
        <span className={`inline-block border px-1.5 py-[1px] font-mono text-[10px] ${evalStatusClass(row.run, blocked)}`}>
          {blocked ? "blocked" : formatSnakeLabel(row.run.status)}
        </span>
        {row.run.source_task_id ? (
          <div className="mt-1 truncate font-mono text-[10px] text-ink-4" title={row.run.source_task_id}>
            task {row.run.source_task_id}
          </div>
        ) : null}
      </td>
      <td className="px-3 py-2">
        {snapshot ? (
          <div className="flex items-center gap-2 font-mono text-[11px]">
            <span className={`border px-1.5 py-[1px] text-[10px] ${gradeClass(snapshot.final_grade)}`}>
              {snapshot.final_grade}
            </span>
            <span className="text-ink">{snapshot.final_score}/100</span>
          </div>
        ) : (
          <span className="font-mono text-[11px] text-ink-4">N/A</span>
        )}
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">{gateLabel(snapshot)}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
        <div>{fmtInt(totals.tokens)} tokens</div>
        <div className="mt-1 text-[10px] text-ink-4">{formatCost(totals.micros)}</div>
      </td>
      <td className="px-3 py-2">
        {row.quality_snapshot_error ? (
          <div className="line-clamp-2 text-[11px] leading-snug text-rust" title={row.quality_snapshot_error}>
            {row.quality_snapshot_error}
          </div>
        ) : blockers.length > 0 ? (
          <div className="line-clamp-2 text-[11px] leading-snug text-rust" title={blockers[0]}>
            {blockers[0]}
          </div>
        ) : (
          <span className="font-mono text-[11px] text-ink-4">None</span>
        )}
      </td>
    </tr>
  );
}
