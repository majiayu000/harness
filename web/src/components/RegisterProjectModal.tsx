import { useState } from "react";
import { registerProject } from "@/lib/queries";
import { ApiError } from "@/lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
  onSuccess: () => void;
}

export function RegisterProjectModal({ open, onClose, onSuccess }: Props) {
  const [id, setId] = useState("");
  const [root, setRoot] = useState("");
  const [defaultAgent, setDefaultAgent] = useState<"claude" | "codex">("claude");
  const [maxConcurrent, setMaxConcurrent] = useState(8);
  const [busy, setBusy] = useState(false);
  const [errors, setErrors] = useState<Record<string, string>>({});

  if (!open) return null;

  function validate(): Record<string, string> {
    const errs: Record<string, string> = {};
    if (!id.trim()) {
      errs.id = "Project ID is required";
    } else if (!/^[a-z0-9-]+$/.test(id.trim())) {
      errs.id = "ID must contain only lowercase letters, numbers, and hyphens";
    }
    if (!root.trim()) {
      errs.root = "Root path is required";
    }
    if (!Number.isInteger(maxConcurrent) || maxConcurrent < 1) {
      errs.maxConcurrent = "Must be a positive integer";
    }
    return errs;
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const errs = validate();
    if (Object.keys(errs).length > 0) {
      setErrors(errs);
      return;
    }
    setErrors({});
    setBusy(true);
    try {
      await registerProject({
        id: id.trim(),
        root: root.trim(),
        default_agent: defaultAgent,
        max_concurrent: maxConcurrent,
      });
      onSuccess();
      onClose();
    } catch (err) {
      if (err instanceof ApiError) {
        setErrors({ form: `Error: ${err.message}` });
      } else {
        setErrors({ form: (err as Error).message });
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-[9999]">
      <div className="bg-bg-1 border border-line-2 min-w-[420px] font-sans">
        <div className="p-5 border-b border-line-2">
          <p className="m-0 font-semibold text-ink">Register project</p>
        </div>
        <form onSubmit={handleSubmit} className="p-5 space-y-4">
          <div>
            <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Project ID</label>
            <input
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="my-project"
              className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
            />
            {errors.id && <div className="font-mono text-[11px] text-red-400 mt-1">{errors.id}</div>}
          </div>
          <div>
            <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Root path</label>
            <input
              value={root}
              onChange={(e) => setRoot(e.target.value)}
              placeholder="/srv/repos/my-project"
              className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
            />
            {root.length > 0 && !root.startsWith("/") && (
              <div className="font-mono text-[11px] text-ink-3 mt-1">
                Path should be absolute — server will verify existence
              </div>
            )}
            {errors.root && <div className="font-mono text-[11px] text-red-400 mt-1">{errors.root}</div>}
          </div>
          <div>
            <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Default agent</label>
            <select
              value={defaultAgent}
              onChange={(e) => setDefaultAgent(e.target.value as "claude" | "codex")}
              className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
            >
              <option value="claude">claude</option>
              <option value="codex">codex</option>
            </select>
          </div>
          <div>
            <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Max concurrent</label>
            <input
              type="number"
              value={maxConcurrent}
              onChange={(e) => setMaxConcurrent(Number(e.target.value))}
              min={1}
              className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
            />
            {errors.maxConcurrent && (
              <div className="font-mono text-[11px] text-red-400 mt-1">{errors.maxConcurrent}</div>
            )}
          </div>
          {errors.form && <div className="font-mono text-[11px] text-red-400">{errors.form}</div>}
          <div className="flex gap-2 justify-end pt-2">
            <button
              type="button"
              onClick={onClose}
              disabled={busy}
              className="px-3.5 py-1.5 border border-line-2 text-ink font-mono text-[12px] disabled:opacity-60 cursor-pointer"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={busy}
              className="px-3.5 py-1.5 bg-rust text-white border-0 font-mono text-[12px] disabled:opacity-60 cursor-pointer"
            >
              {busy ? "Registering…" : "Register"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
