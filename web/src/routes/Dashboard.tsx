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
import { useDashboard } from "@/lib/queries";

type Tab = "board" | "history" | "channels" | "submit";

export function Dashboard() {
  const [tab, setTab] = useState<Tab>("board");
  const { isError } = useDashboard();
  const [searchParams] = useSearchParams();
  const projectFilter = searchParams.get("project");

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
        onItemClick={(id) => setTab(id as Tab)}
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
          {tab === "board" && <Active projectFilter={projectFilter} />}
          {tab === "history" && <History projectFilter={projectFilter} />}
          {tab === "channels" && <Channels projectFilter={projectFilter} />}
          {tab === "submit" && <Submit projectFilter={projectFilter} />}
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
