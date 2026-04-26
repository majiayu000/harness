import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { OverviewProject } from "@/types";
import { fmtInt, fmtScore } from "@/lib/format";
import { RegisterProjectDialog } from "./RegisterProjectDialog";

interface Props {
  projects: OverviewProject[];
}

function scoreClass(score: number | null): string {
  if (score === null || !Number.isFinite(score)) return "";
  if (score >= 85) return "text-ok border-ok/40";
  if (score >= 70) return "text-warn border-warn/40";
  return "";
}

function trendSvg(arr: number[]): string {
  if (!arr.length) return "";
  const w = 58, h = 20;
  const mx = Math.max(...arr), mn = Math.min(...arr);
  const denom = mx - mn || 1;
  return arr
    .map((v, i) => `${(i / Math.max(1, arr.length - 1)) * w},${h - ((v - mn) / denom) * (h - 2) - 1}`)
    .join(" ");
}

export function ProjectsTable({ projects }: Props) {
  const nav = useNavigate();
  const [showRegister, setShowRegister] = useState(false);

  if (!projects.length) {
    return (
      <>
        <div className="px-5 py-8 text-center">
          <p className="font-mono text-[12px] text-ink-3 mb-3">No projects registered yet</p>
          <button
            onClick={() => setShowRegister(true)}
            className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0"
          >
            Register a project
          </button>
        </div>
        <RegisterProjectDialog
          open={showRegister}
          onClose={() => setShowRegister(false)}
        />
      </>
    );
  }

  return (
    <table className="w-full border-collapse text-[13px]">
      <thead>
        <tr>
          {["Project", "Pipeline", "Merged 24h", "Avg score", "Trend", "Worktrees", "Tokens 24h", "Agents", ""].map(
            (h) => (
              <th
                key={h}
                className="font-mono text-[10.5px] text-ink-3 uppercase tracking-[0.08em] font-medium text-left px-4 py-2.5 border-b border-line bg-bg sticky top-[41px] z-0"
              >
                {h}
              </th>
            ),
          )}
        </tr>
      </thead>
      <tbody>
        {projects.map((p) => {
          const total = p.running + 0 + p.queued;
          const rw = total ? (p.running / total) * 100 : 0;
          const rq = total ? (p.queued / total) * 100 : 0;
          return (
            <tr
              key={p.id}
              className="cursor-pointer hover:bg-bg-1 transition-colors"
              onClick={() => nav("/?project=" + encodeURIComponent(p.id))}
            >
              <td className="px-4 py-3 border-b border-line align-middle">
                <div className="flex items-center gap-2.5">
                  <div className="w-7 h-7 bg-bg-3 border border-line-2 grid place-items-center font-mono font-semibold text-rust text-[11px] flex-none">
                    {p.id.slice(0, 1).toUpperCase()}
                  </div>
                  <div>
                    <div className="font-medium text-ink">{p.id}</div>
                    <div className="font-mono text-[11px] text-ink-3">{p.root}</div>
                  </div>
                </div>
              </td>
              <td className="px-4 py-3 border-b border-line align-middle">
                <div className="w-[120px] h-1.5 bg-bg-2 border border-line relative overflow-hidden">
                  <i className="absolute top-0 bottom-0 bg-rust left-0" style={{ width: `${rw}%` }} />
                  <i className="absolute top-0 bottom-0 bg-ink-4" style={{ left: `${rw}%`, width: `${rq}%` }} />
                </div>
                <div className="text-ink-3 mt-1 text-[11px] font-mono">
                  {p.running} running · {p.queued} queued
                </div>
              </td>
              <td className="px-4 py-3 border-b border-line align-middle font-mono text-[12px] text-ink-2">{fmtInt(p.merged_24h)}</td>
              <td className="px-4 py-3 border-b border-line align-middle">
                <span className={`font-mono text-[12.5px] px-[7px] py-0.5 border rounded-[3px] text-ink ${scoreClass(p.avg_score)}`}>
                  {fmtScore(p.avg_score)}
                </span>
              </td>
              <td className="px-4 py-3 border-b border-line align-middle text-rust">
                <svg className="inline-block w-[58px] h-5 align-[-4px]" viewBox="0 0 58 20" preserveAspectRatio="none">
                  <polyline fill="none" stroke="currentColor" strokeWidth="1.3" points={trendSvg(p.trend)} />
                </svg>
              </td>
              <td className="px-4 py-3 border-b border-line align-middle font-mono text-[12px] text-ink-2">{p.worktrees ?? "—"}</td>
              <td className="px-4 py-3 border-b border-line align-middle font-mono text-[12px] text-ink-2">{p.tokens_24h ?? "—"}</td>
              <td className="px-4 py-3 border-b border-line align-middle font-mono text-[12px] text-ink-3">
                {p.agents.length ? p.agents.join(" · ") : "—"}
              </td>
              <td className="px-4 py-3 border-b border-line align-middle">
                <span className="text-ink-3 font-mono">→</span>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
