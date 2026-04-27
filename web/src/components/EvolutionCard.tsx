import type { OverviewEvolution } from "@/types/overview";
import { Panel } from "./Panel";
import { fmtInt } from "@/lib/format";

interface Props {
  evolution: OverviewEvolution | null;
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between py-1.5 border-b border-line last:border-b-0">
      <span className="text-ink-3">{label}</span>
      <span className="text-ink font-medium">{value}</span>
    </div>
  );
}

export function EvolutionCard({ evolution }: Props) {
  const fmt = (n: number | undefined) => (n != null ? fmtInt(n) : "—");
  return (
    <Panel title="Self-evolution" sub="last tick telemetry">
      <div className="px-5 py-3 font-mono text-[11px]">
        <Row label="drafts pending" value={fmt(evolution?.drafts_pending)} />
        <Row label="auto-adopted · 24h" value={fmt(evolution?.drafts_auto_adopted)} />
        <Row label="skills invoked · 24h" value={fmt(evolution?.skills_invoked_in_window)} />
      </div>
    </Panel>
  );
}
