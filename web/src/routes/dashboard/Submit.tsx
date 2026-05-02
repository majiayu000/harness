import { useState, useEffect } from "react";
import { useOverview } from "@/lib/queries";
import { apiFetch } from "@/lib/api";
import type { CreateTaskResponse } from "@/types";
import { SubmitSuccess } from "./SubmitSuccess";

type InputMode = "issue" | "pr" | "prompt";
type WizardStep = "mode" | "config" | "options" | "success";

interface WizardState {
  mode: InputMode | null;
  issueInput: string;
  prInput: string;
  promptInput: string;
  project: string;
  agent: string;
  maxTurns: string;
  turnTimeoutSecs: string;
}

interface Props {
  projectFilter?: string | null;
}

interface CreatedSubmission {
  taskId: string;
  workflowId: string | null;
  executionPath: string | null;
}

function parsePositiveInteger(input: string): number | null {
  const trimmed = input.trim();
  if (!/^[1-9]\d*$/.test(trimmed)) return null;
  const value = Number(trimmed);
  return Number.isSafeInteger(value) ? value : null;
}

function parseNonNegativeInteger(input: string): number | null {
  const trimmed = input.trim();
  if (!/^(0|[1-9]\d*)$/.test(trimmed)) return null;
  const value = Number(trimmed);
  return Number.isSafeInteger(value) ? value : null;
}

function parseIssueNumber(input: string): number | null {
  const trimmed = input.trim();
  const direct = parsePositiveInteger(trimmed);
  if (direct !== null) return direct;
  const match = trimmed.match(/\/issues\/(\d+)/);
  if (match) return parsePositiveInteger(match[1]);
  return null;
}

function parsePrNumber(input: string): number | null {
  const trimmed = input.trim();
  const direct = parsePositiveInteger(trimmed);
  if (direct !== null) return direct;
  const match = trimmed.match(/\/pull\/(\d+)/);
  if (match) return parsePositiveInteger(match[1]);
  return null;
}

// ── Step: Mode ────────────────────────────────────────────────────────────────

interface StepModeProps {
  mode: InputMode | null;
  onChange: (mode: InputMode) => void;
  onNext: () => void;
}

function StepMode({ mode, onChange, onNext }: StepModeProps) {
  const MODES: { id: InputMode; label: string; tooltip: string }[] = [
    { id: "issue", label: "Issue", tooltip: "Run agent on a GitHub issue number" },
    { id: "pr", label: "Pull Request", tooltip: "Run agent on a GitHub pull request number" },
    { id: "prompt", label: "Prompt", tooltip: "Run agent on a free-form text prompt" },
  ];
  return (
    <div className="space-y-4">
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-2">
          Input mode
        </label>
        <div className="flex gap-2">
          {MODES.map((m) => (
            <button
              key={m.id}
              type="button"
              title={m.tooltip}
              onClick={() => onChange(m.id)}
              className={`flex-1 py-3 px-4 border font-mono text-[12px] rounded-[3px] text-left ${
                mode === m.id
                  ? "border-rust text-rust bg-rust/5"
                  : "border-line-2 text-ink-2 hover:border-line hover:text-ink hover:bg-bg-2"
              }`}
            >
              {m.label}
            </button>
          ))}
        </div>
      </div>
      <button
        type="button"
        disabled={!mode}
        onClick={onNext}
        className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
      >
        Next →
      </button>
    </div>
  );
}

// ── Step: Config ──────────────────────────────────────────────────────────────

interface StepConfigProps {
  state: WizardState;
  projects: { id: string; agents: string[] }[];
  projectsLoading: boolean;
  onChange: (patch: Partial<WizardState>) => void;
  onNext: () => void;
  onBack: () => void;
}

function StepConfig({
  state,
  projects,
  projectsLoading,
  onChange,
  onNext,
  onBack,
}: StepConfigProps) {
  const agents = projects.find((p) => p.id === state.project)?.agents ?? [];

  const projectSelected = !projectsLoading && (projects.length === 0 || state.project !== "");
  const isValid =
    projectSelected &&
    ((state.mode === "issue" && parseIssueNumber(state.issueInput) !== null) ||
      (state.mode === "pr" && parsePrNumber(state.prInput) !== null) ||
      (state.mode === "prompt" && state.promptInput.trim().length > 0));

  return (
    <div className="space-y-4">
      <div>
        <label
          htmlFor="wizard-project"
          className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1"
        >
          Project
        </label>
        {projectsLoading ? (
          <p className="font-mono text-[11px] text-ink-3 py-1">Loading projects…</p>
        ) : projects.length === 0 ? (
          <p className="font-mono text-[11px] text-ink-3 py-1">No projects registered</p>
        ) : (
          <select
            id="wizard-project"
            value={state.project}
            onChange={(e) => onChange({ project: e.target.value, agent: "" })}
            className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          >
            <option value="">— select project —</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.id}
              </option>
            ))}
          </select>
        )}
      </div>
      <div>
        <label
          htmlFor="wizard-agent"
          className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1"
        >
          Agent
        </label>
        <select
          id="wizard-agent"
          value={state.agent}
          onChange={(e) => onChange({ agent: e.target.value })}
          disabled={agents.length === 0}
          className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px] disabled:opacity-60"
        >
          <option value="">— auto-select —</option>
          {agents.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
        </select>
      </div>
      {state.mode === "issue" && (
        <div>
          <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Issue number or URL
          </label>
          <input
            value={state.issueInput}
            onChange={(e) => onChange({ issueInput: e.target.value })}
            placeholder="123 or https://github.com/owner/repo/issues/123"
            className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          />
        </div>
      )}
      {state.mode === "pr" && (
        <div>
          <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            PR number or URL
          </label>
          <input
            value={state.prInput}
            onChange={(e) => onChange({ prInput: e.target.value })}
            placeholder="456 or https://github.com/owner/repo/pull/456"
            className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          />
        </div>
      )}
      {state.mode === "prompt" && (
        <div>
          <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
            Prompt
          </label>
          <textarea
            value={state.promptInput}
            onChange={(e) => onChange({ promptInput: e.target.value })}
            rows={4}
            className="w-full bg-bg border border-line-2 px-2.5 py-2 text-ink font-mono text-[12px] rounded-[3px]"
          />
        </div>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={onBack}
          className="px-3 py-1.5 bg-bg-2 text-ink-2 font-mono text-[12px] border border-line-2 rounded-[3px]"
        >
          ← Back
        </button>
        <button
          type="button"
          disabled={!isValid}
          onClick={onNext}
          className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
        >
          Next →
        </button>
      </div>
    </div>
  );
}

// ── Step: Options ─────────────────────────────────────────────────────────────

interface StepOptionsProps {
  state: WizardState;
  onChange: (patch: Partial<WizardState>) => void;
  onSubmit: () => void;
  onBack: () => void;
  busy: boolean;
  error: string | null;
}

function StepOptions({ state, onChange, onSubmit, onBack, busy, error }: StepOptionsProps) {
  return (
    <div className="space-y-4">
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
          Max turns (optional)
        </label>
        <input
          type="number"
          min="1"
          value={state.maxTurns}
          onChange={(e) => onChange({ maxTurns: e.target.value })}
          placeholder="leave blank for default"
          className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
      </div>
      <div>
        <label className="block font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 mb-1">
          Turn timeout seconds (optional)
        </label>
        <input
          type="number"
          min="0"
          value={state.turnTimeoutSecs}
          onChange={(e) => onChange({ turnTimeoutSecs: e.target.value })}
          placeholder="leave blank for default"
          className="w-full h-[30px] bg-bg border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
      </div>
      {error && (
        <div className="px-3 py-2 border border-danger/40 text-danger font-mono text-[12px] rounded-[3px] bg-danger/5">
          {error}
        </div>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={onBack}
          className="px-3 py-1.5 bg-bg-2 text-ink-2 font-mono text-[12px] border border-line-2 rounded-[3px]"
        >
          ← Back
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={onSubmit}
          className="px-3 py-1.5 bg-rust text-white font-mono text-[12px] border-0 disabled:opacity-60"
        >
          {busy ? "Submitting…" : "Submit Task"}
        </button>
      </div>
    </div>
  );
}

// ── Main wizard ───────────────────────────────────────────────────────────────

const STEP_LABELS: Record<Exclude<WizardStep, "success">, string> = {
  mode: "1. Mode",
  config: "2. Config",
  options: "3. Options",
};

export function Submit({ projectFilter }: Props) {
  const [step, setStep] = useState<WizardStep>("mode");
  const [wizardState, setWizardState] = useState<WizardState>({
    mode: null,
    issueInput: "",
    prInput: "",
    promptInput: "",
    project: projectFilter ?? "",
    agent: "",
    maxTurns: "",
    turnTimeoutSecs: "",
  });
  const [createdSubmission, setCreatedSubmission] = useState<CreatedSubmission | null>(null);
  const [busy, setBusy] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const { data: overview, isLoading: projectsLoading = false } = useOverview();
  const projects = overview?.projects ?? [];

  useEffect(() => {
    setWizardState((prev) => ({ ...prev, project: projectFilter ?? "" }));
  }, [projectFilter]);

  function patchState(patch: Partial<WizardState>) {
    setWizardState((prev) => ({ ...prev, ...patch }));
  }

  function handleModeChange(mode: InputMode) {
    patchState({
      mode,
      issueInput: mode === "issue" ? wizardState.issueInput : "",
      prInput: mode === "pr" ? wizardState.prInput : "",
      promptInput: mode === "prompt" ? wizardState.promptInput : "",
    });
  }

  async function handleSubmit() {
    if (!wizardState.mode) return;
    setBusy(true);
    setSubmitError(null);

    const common: Record<string, unknown> = {};
    if (wizardState.project) common.project = wizardState.project;
    if (wizardState.agent) common.agent = wizardState.agent;
    if (wizardState.maxTurns.trim() !== "") {
      const n = parsePositiveInteger(wizardState.maxTurns);
      if (n === null) {
        setSubmitError("Max turns must be a positive integer");
        setBusy(false);
        return;
      }
      common.max_turns = n;
    }
    if (wizardState.turnTimeoutSecs.trim() !== "") {
      const n = parseNonNegativeInteger(wizardState.turnTimeoutSecs);
      if (n === null) {
        setSubmitError("Turn timeout seconds must be a non-negative integer");
        setBusy(false);
        return;
      }
      common.turn_timeout_secs = n;
    }

    let modePayload: Record<string, unknown>;
    if (wizardState.mode === "issue") {
      const num = parseIssueNumber(wizardState.issueInput);
      if (num === null) {
        setSubmitError("Invalid issue number");
        setBusy(false);
        return;
      }
      modePayload = { issue: num };
    } else if (wizardState.mode === "pr") {
      const num = parsePrNumber(wizardState.prInput);
      if (num === null) {
        setSubmitError("Invalid PR number");
        setBusy(false);
        return;
      }
      modePayload = { pr: num };
    } else {
      modePayload = { prompt: wizardState.promptInput.trim() };
    }

    try {
      const resp = await apiFetch("/tasks", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...modePayload, ...common }),
      });
      const json = (await resp.json()) as CreateTaskResponse;
      setCreatedSubmission({
        taskId: json.task_id,
        workflowId: json.workflow_id ?? null,
        executionPath: json.execution_path ?? null,
      });
      setStep("success");
    } catch (e) {
      setSubmitError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  function handleReset() {
    setCreatedSubmission(null);
    setStep("mode");
    setWizardState({
      mode: null,
      issueInput: "",
      prInput: "",
      promptInput: "",
      project: projectFilter ?? "",
      agent: "",
      maxTurns: "",
      turnTimeoutSecs: "",
    });
    setSubmitError(null);
  }

  return (
    <div className="max-w-[640px] space-y-5">
      {step !== "success" && (
        <div className="flex gap-4 font-mono text-[10.5px]">
          {(["mode", "config", "options"] as Exclude<WizardStep, "success">[]).map((s) => (
            <span key={s} className={step === s ? "text-rust font-medium" : "text-ink-3"}>
              {STEP_LABELS[s]}
            </span>
          ))}
        </div>
      )}
      <div className="border border-line bg-bg-1 p-5">
        {step === "mode" && (
          <StepMode
            mode={wizardState.mode}
            onChange={handleModeChange}
            onNext={() => setStep("config")}
          />
        )}
        {step === "config" && (
          <StepConfig
            state={wizardState}
            projects={projects}
            projectsLoading={projectsLoading}
            onChange={patchState}
            onNext={() => setStep("options")}
            onBack={() => setStep("mode")}
          />
        )}
        {step === "options" && (
          <StepOptions
            state={wizardState}
            onChange={patchState}
            onSubmit={handleSubmit}
            onBack={() => setStep("config")}
            busy={busy}
            error={submitError}
          />
        )}
        {step === "success" && createdSubmission && (
          <SubmitSuccess
            taskId={createdSubmission.taskId}
            workflowId={createdSubmission.workflowId}
            executionPath={createdSubmission.executionPath}
            onReset={handleReset}
          />
        )}
      </div>
    </div>
  );
}
