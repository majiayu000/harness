import { useState, useEffect } from "react";
import { apiFetch } from "@/lib/api";
import { useProjects } from "@/lib/queries";

const TASK_TEMPLATES = [
  {
    label: "Fix a bug",
    title: "Fix a bug",
    description:
      "Describe the bug you want fixed, including steps to reproduce and expected behavior.",
  },
  {
    label: "Add a feature",
    title: "Add a feature",
    description:
      "Describe the feature you want added, including the motivation and expected behavior.",
  },
  {
    label: "Write tests",
    title: "Write tests",
    description:
      "Describe which code paths or behaviors need test coverage and what types of tests are needed.",
  },
];

interface Props {
  projectFilter?: string | null;
  onTaskCreated?: (id: string) => void;
}

export function Submit({ projectFilter, onTaskCreated }: Props) {
  const [title, setTitle] = useState("");
  const [desc, setDesc] = useState("");
  const [project, setProject] = useState(projectFilter ?? "");
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { data: projects = [] } = useProjects();

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
      setTitle("");
      setDesc("");
      setProject(projectFilter ?? "");
      if (onTaskCreated) {
        onTaskCreated(json.id ?? "");
      } else {
        setMsg(`created task ${json.id ?? "?"}`);
      }
    } catch (e) {
      setMsg((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="max-w-[640px] space-y-4">
      <div>
        <p className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-2">
          Starter templates
        </p>
        <div className="flex gap-2 flex-wrap">
          {TASK_TEMPLATES.map((t) => (
            <button
              key={t.label}
              type="button"
              onClick={() => {
                setTitle(t.title);
                setDesc(t.description);
              }}
              className="px-2.5 py-1 border border-line-2 bg-bg font-mono text-[11px] text-ink-2 hover:bg-bg-1 transition-colors"
            >
              {t.label}
            </button>
          ))}
        </div>
      </div>
      <form onSubmit={submit} className="border border-line bg-bg-1 p-5 space-y-4">
        <div>
          <label htmlFor="submit-project" className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Project
          </label>
          <select
            id="submit-project"
            value={project}
            onChange={(e) => setProject(e.target.value)}
            className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          >
            <option value="">Select a project…</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.id}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label htmlFor="submit-title" className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Title
          </label>
          <input
            id="submit-title"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            required
            className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          />
        </div>
        <div>
          <label htmlFor="submit-desc" className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Description
          </label>
          <textarea
            id="submit-desc"
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
    </div>
  );
}
