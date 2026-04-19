import { useState, useEffect } from "react";
import { apiFetch } from "@/lib/api";

interface Props {
  projectFilter?: string | null;
}

export function Submit({ projectFilter }: Props) {
  const [title, setTitle] = useState("");
  const [desc, setDesc] = useState("");
  const [project, setProject] = useState(projectFilter ?? "");
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setProject(projectFilter ?? "");
  }, [projectFilter]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!title.trim() || !desc.trim()) return;
    setBusy(true);
    setMsg(null);
    try {
      const prompt = title.trim() + (desc.trim() ? `\n\n${desc.trim()}` : "");
      const body = JSON.stringify({ prompt, project: project.trim() || undefined });
      const resp = await apiFetch("/tasks", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body,
      });
      const json = await resp.json();
      setMsg(`created task ${json.id ?? "?"}`);
      setTitle("");
      setDesc("");
      setProject(projectFilter ?? "");
    } catch (e) {
      setMsg((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={submit} className="max-w-[640px] border border-line bg-bg-1 p-5 space-y-4">
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Project</label>
        <input
          value={project}
          onChange={(e) => setProject(e.target.value)}
          placeholder="project id (optional)"
          className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
      </div>
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Title</label>
        <input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          required
          className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
      </div>
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">Description</label>
        <textarea
          value={desc}
          onChange={(e) => setDesc(e.target.value)}
          required
          rows={4}
          className="w-full bg-bg border border-line-2 px-2.5 py-2 text-ink font-mono text-[12px] rounded-[3px]"
        />
      </div>
      <button
        disabled={busy}
        type="submit"
        className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
      >
        {busy ? "Submitting…" : "Submit Task"}
      </button>
      {msg && <div className="font-mono text-[11px] text-ink-2">{msg}</div>}
    </form>
  );
}
