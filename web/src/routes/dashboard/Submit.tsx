import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  useCreateTask,
  useDashboard,
  useProjects,
  useRegisterProject,
  useValidateProject,
} from "@/lib/queries";

interface Props {
  projectFilter?: string | null;
}

interface StarterTemplate {
  id: string;
  label: string;
  title: string;
  description: string;
}

const STARTER_TEMPLATES: StarterTemplate[] = [
  {
    id: "readme",
    label: "Polish README",
    title: "Improve the onboarding docs",
    description: "Tighten the first-run documentation, keep the scope minimal, and verify the operator flow still matches the product.",
  },
  {
    id: "test",
    label: "Add coverage",
    title: "Add focused regression coverage",
    description: "Find the smallest missing test for a recent behavior change, implement it, and keep the rest of the code untouched.",
  },
  {
    id: "bugfix",
    label: "Fix a bug",
    title: "Investigate and fix a concrete bug",
    description: "Diagnose the root cause first, then apply the smallest safe fix and validate it with the relevant tests.",
  },
];

function projectLabel(project: { id: string; name?: string | null; root: string }): string {
  return project.name?.trim() || project.root.split("/").filter(Boolean).at(-1) || project.id;
}

export function Submit({ projectFilter }: Props) {
  const navigate = useNavigate();
  const dashboard = useDashboard();
  const projects = useProjects();
  const validateProject = useValidateProject();
  const registerProject = useRegisterProject();
  const createTask = useCreateTask();

  const [root, setRoot] = useState("");
  const [selectedProject, setSelectedProject] = useState("");
  const [title, setTitle] = useState("");
  const [desc, setDesc] = useState("");
  const [notice, setNotice] = useState<string | null>(null);

  const availableProjects = projects.data ?? [];
  const canSubmit = selectedProject.trim() && title.trim() && desc.trim();
  const activeValidation =
    validateProject.data && root.trim() === validateProject.variables?.root ? validateProject.data : null;

  useEffect(() => {
    if (!availableProjects.length) {
      setSelectedProject("");
      return;
    }
    if (projectFilter) {
      const match = availableProjects.find(
        (project) => project.id === projectFilter || project.root === projectFilter,
      );
      if (match) {
        setSelectedProject(match.id);
        return;
      }
    }
    setSelectedProject((current) => current || availableProjects[0].id);
  }, [availableProjects, projectFilter]);

  const onboardingCopy = useMemo(() => {
    const phase = dashboard.data?.onboarding.phase;
    if (phase === "register_project") {
      return "Register the repository you want Harness to operate on.";
    }
    if (phase === "submit_task") {
      return "Pick a registered project and start with one focused task.";
    }
    if (phase === "watch_live_output") {
      return "Your first task is queued. Open it and watch the live output.";
    }
    if (phase === "inspect_completion") {
      return "The run produced output. Reopen it from history to inspect the evidence.";
    }
    return "Compose the next operator task.";
  }, [dashboard.data?.onboarding.phase]);

  async function handleValidateProject() {
    setNotice(null);
    try {
      await validateProject.mutateAsync({ root: root.trim() });
    } catch (error) {
      setNotice((error as Error).message);
    }
  }

  async function handleRegisterProject() {
    setNotice(null);
    try {
      const response = await registerProject.mutateAsync({ root: root.trim() });
      setSelectedProject(response.project.id);
      setNotice(
        response.status === "existing"
          ? "Project already existed. Using the registered record."
          : "Project registered. You can submit a task now.",
      );
    } catch (error) {
      setNotice((error as Error).message);
    }
  }

  async function handleSubmit(event: React.FormEvent) {
    event.preventDefault();
    if (!canSubmit) return;
    setNotice(null);
    try {
      const prompt = title.trim() + (desc.trim() ? `\n\n${desc.trim()}` : "");
      const response = await createTask.mutateAsync({
        prompt,
        project: selectedProject || undefined,
      });
      setTitle("");
      setDesc("");
      navigate(`/?task=${encodeURIComponent(response.task_id)}`);
    } catch (error) {
      setNotice((error as Error).message);
    }
  }

  function applyTemplate(template: StarterTemplate) {
    setTitle(template.title);
    setDesc(template.description);
  }

  return (
    <div className="space-y-5">
      <section className="border border-line bg-bg-1 p-5 space-y-4">
        <div>
          <div className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">
            First run
          </div>
          <h2 className="mt-2 text-[18px] text-ink">Setup and submit</h2>
          <p className="mt-2 text-[13px] text-ink-2 max-w-[720px]">{onboardingCopy}</p>
        </div>

        <div className="grid gap-3 lg:grid-cols-[minmax(0,1fr)_auto_auto]">
          <input
            value={root}
            onChange={(event) => setRoot(event.target.value)}
            placeholder="/absolute/path/to/your/repository"
            className="h-[34px] bg-bg border border-line-2 px-3 text-ink font-mono text-[12px] rounded-[3px]"
          />
          <button
            type="button"
            onClick={handleValidateProject}
            disabled={!root.trim() || validateProject.isPending}
            className="px-3 py-1.5 border border-line-2 bg-bg text-ink font-mono text-[12px] disabled:opacity-60"
          >
            {validateProject.isPending ? "Validating…" : "Validate"}
          </button>
          <button
            type="button"
            onClick={handleRegisterProject}
            disabled={!root.trim() || registerProject.isPending}
            className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
          >
            {registerProject.isPending ? "Registering…" : "Register project"}
          </button>
        </div>

        {activeValidation ? (
          <div className="grid gap-3 rounded-[4px] border border-line bg-bg px-4 py-3 text-[12px] text-ink-2 md:grid-cols-3">
            <div>
              <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Detected id</div>
              <div className="mt-1 break-all">{activeValidation.project_id}</div>
            </div>
            <div>
              <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Canonical root</div>
              <div className="mt-1 break-all">{activeValidation.canonical_root}</div>
            </div>
            <div>
              <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Detected repo</div>
              <div className="mt-1">{activeValidation.repo ?? activeValidation.display_name}</div>
            </div>
          </div>
        ) : null}
      </section>

      <form onSubmit={handleSubmit} className="border border-line bg-bg-1 p-5 space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <div className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">
              Guided submit
            </div>
            <p className="mt-2 text-[13px] text-ink-2">
              Use a registered project, start with one focused task, then jump straight into the live task view.
            </p>
          </div>
          <div className="flex flex-wrap gap-2">
            {STARTER_TEMPLATES.map((template) => (
              <button
                key={template.id}
                type="button"
                onClick={() => applyTemplate(template)}
                className="px-2.5 py-1 border border-line-2 bg-bg text-ink font-mono text-[11px]"
              >
                {template.label}
              </button>
            ))}
          </div>
        </div>

        <div>
          <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Project
          </label>
          <select
            aria-label="Project"
            value={selectedProject}
            onChange={(event) => setSelectedProject(event.target.value)}
            disabled={!availableProjects.length}
            className="w-full h-[34px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px] disabled:opacity-60"
          >
            {!availableProjects.length ? (
              <option value="">Register a project first</option>
            ) : null}
            {availableProjects.map((project) => (
              <option key={project.id} value={project.id}>
                {projectLabel(project)}
              </option>
            ))}
          </select>
        </div>

        <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_260px]">
          <div className="space-y-4">
            <div>
              <label htmlFor="task-title" className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
                Title
              </label>
              <input
                id="task-title"
                value={title}
                onChange={(event) => setTitle(event.target.value)}
                required
                className="w-full h-[34px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
              />
            </div>
            <div>
              <label htmlFor="task-description" className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
                Description
              </label>
              <textarea
                id="task-description"
                value={desc}
                onChange={(event) => setDesc(event.target.value)}
                required
                rows={5}
                className="w-full bg-bg border border-line-2 px-2.5 py-2 text-ink font-mono text-[12px] rounded-[3px]"
              />
            </div>
          </div>

          <div className="border border-line-2 bg-bg p-3">
            <div className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">
              Preflight hints
            </div>
            <div className="mt-2 space-y-2 text-[12px] text-ink-2">
              <div>Anchor the task to one concrete outcome.</div>
              <div>Call out the failing behavior or missing evidence.</div>
              <div>Keep the first run narrow enough to finish in one sitting.</div>
            </div>
          </div>
        </div>

        <button
          disabled={!canSubmit || createTask.isPending}
          type="submit"
          className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
        >
          {createTask.isPending ? "Submitting…" : "Submit task"}
        </button>
      </form>

      {notice ? <div className="font-mono text-[11px] text-ink-2">{notice}</div> : null}
    </div>
  );
}
