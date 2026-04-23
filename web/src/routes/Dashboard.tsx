import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { StatusBadge } from "@/components/StatusBadge";
import { PaletteFab } from "@/components/PaletteFab";
import { Active } from "./dashboard/Active";
import { History } from "./dashboard/History";
import { Channels } from "./dashboard/Channels";
import { Submit } from "./dashboard/Submit";
import { useDashboard, useTasks } from "@/lib/queries";

type Tab = "board" | "history" | "channels" | "submit";

export function Dashboard() {
  const [manualTab, setManualTab] = useState<Tab | null>(null);
  const { data: dashboard, isError } = useDashboard();
  const { data: tasks } = useTasks();
  const [searchParams, setSearchParams] = useSearchParams();
  const projectFilter = searchParams.get("project");
  const selectedTaskId = searchParams.get("task");
  const selectedTask = (tasks ?? []).find((task) => task.id === selectedTaskId) ?? null;
  const selectedTaskTerminal = selectedTask
    ? ["done", "failed", "cancelled"].includes(selectedTask.status)
    : false;
  const derivedTab: Tab = selectedTaskId
    ? selectedTaskTerminal
      ? "history"
      : "board"
    : dashboard?.onboarding.phase === "register_project" ||
        dashboard?.onboarding.phase === "submit_task"
      ? "submit"
      : "board";
  const tab = manualTab ?? derivedTab;

  function handleSelectTask(taskId: string) {
    const next = new URLSearchParams(searchParams);
    next.set("task", taskId);
    setSearchParams(next);
  }

  const sections: SidebarSection[] = [
    {
      label: "Operations",
      items: [
        { id: "board", label: "Active", active: tab === "board" },
        { id: "history", label: "History", active: tab === "history" },
        { id: "channels", label: "Channels", active: tab === "channels" },
        { id: "submit", label: "Submit", active: tab === "submit" },
      ],
    },
    {
      label: "System",
      items: [{ id: "overview", label: "Overview", href: "/overview" }],
    },
  ];

  return (
    <div className="grid grid-cols-[240px_1fr] h-screen overflow-hidden">
      <Sidebar
        env="local"
        sections={sections}
        onItemClick={(id) => setManualTab(id as Tab)}
      />
      <main className="flex flex-col min-h-0 min-w-0">
        <TopBar
          breadcrumb={
            projectFilter
              ? [{ label: "harness", href: "/overview" }, { label: projectFilter, current: true }]
              : [{ label: "harness" }, { label: "Tasks", current: true }]
          }
          searchPlaceholder="Search tasks…"
          actions={<StatusBadge ok={!isError} />}
        />
        <div className="flex-1 overflow-auto min-h-0 p-6">
          {tab === "board" && (
            <Active
              projectFilter={projectFilter}
              selectedTaskId={selectedTaskId}
              onSelectTask={handleSelectTask}
            />
          )}
          {tab === "history" && (
            <History
              projectFilter={projectFilter}
              selectedTaskId={selectedTaskId}
              onSelectTask={handleSelectTask}
            />
          )}
          {tab === "channels" && <Channels projectFilter={projectFilter} />}
          {tab === "submit" && <Submit projectFilter={projectFilter} />}
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
