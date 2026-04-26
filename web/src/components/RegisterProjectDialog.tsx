import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { apiFetch } from "@/lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
  onSuccess?: (id: string) => void;
}

function deriveId(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "";
}

export function RegisterProjectDialog({ open, onClose, onSuccess }: Props) {
  const [root, setRoot] = useState("");
  const [id, setId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const qc = useQueryClient();

  function handleRootChange(val: string) {
    setRoot(val);
    setId(deriveId(val));
  }

  function handleClose() {
    setRoot("");
    setId("");
    setError(null);
    onClose();
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!root.trim() || !id.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const resp = await apiFetch("/projects", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: id.trim(), root: root.trim() }),
      });
      const json = await resp.json().catch(() => null);
      await qc.invalidateQueries({ queryKey: ["projects"] });
      await qc.invalidateQueries({ queryKey: ["overview"] });
      onSuccess?.(json?.id ?? id.trim());
      handleClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  if (!open) return null;

  return (
    <>
      <div
        className="fixed inset-0 z-[99] bg-black/40"
        onClick={handleClose}
        aria-hidden="true"
        data-testid="dialog-scrim"
      />
      <div className="fixed inset-0 z-[100] flex items-center justify-center pointer-events-none">
        <div
          className="pointer-events-auto bg-bg-1 border border-line-2 p-6 w-[480px] max-w-full space-y-4"
          role="dialog"
          aria-modal="true"
          aria-label="Register project"
        >
          <h2 className="font-mono text-[13px] font-semibold text-ink">Register project</h2>
          <form onSubmit={handleSubmit} className="space-y-4">
            <div>
              <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
                Root path
              </label>
              <input
                value={root}
                onChange={(e) => handleRootChange(e.target.value)}
                placeholder="/path/to/your/project"
                required
                className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
              />
            </div>
            <div>
              <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
                Project ID
              </label>
              <input
                value={id}
                onChange={(e) => setId(e.target.value)}
                placeholder="my-project"
                required
                className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
              />
            </div>
            {error && (
              <div className="font-mono text-[11px] text-rust" role="alert">
                {error}
              </div>
            )}
            <div className="flex gap-3 pt-1">
              <button
                type="submit"
                disabled={busy || !root.trim() || !id.trim()}
                className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
              >
                {busy ? "Registering…" : "Register"}
              </button>
              <button
                type="button"
                onClick={handleClose}
                className="px-3 py-1.5 bg-bg border border-line-2 text-ink font-mono text-[12px]"
              >
                Cancel
              </button>
            </div>
          </form>
        </div>
      </div>
    </>
  );
}
