import { useState } from "react";
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

  const sections: SidebarSection[] = [
    {
      label: "Operations",
      items: [
        { id: "board", label: "Active", active: tab === "board", href: "/" },
        { id: "history", label: "History", active: tab === "history", href: "/" },
        { id: "channels", label: "Channels", active: tab === "channels", href: "/" },
        { id: "submit", label: "Submit", active: tab === "submit", href: "/" },
      ],
    },
    {
      label: "System",
      items: [{ id: "overview", label: "Overview", href: "/overview" }],
    },
  ];

  return (
    <div className="grid grid-cols-[240px_1fr] h-screen overflow-hidden">
      <aside
        onClick={(e) => {
          const a = (e.target as HTMLElement).closest("[data-tab]");
          if (a instanceof HTMLElement) setTab(a.dataset.tab as Tab);
        }}
      >
        <Sidebar env="local" sections={sections} />
      </aside>
      <main className="flex flex-col min-h-0 min-w-0">
        <TopBar
          breadcrumb={[{ label: "harness" }, { label: "Tasks", current: true }]}
          searchPlaceholder="Search tasks…"
          actions={<StatusBadge ok={!isError} />}
        />
        <div className="flex-1 overflow-auto min-h-0 p-6">
          {tab === "board" && <Active />}
          {tab === "history" && <History />}
          {tab === "channels" && <Channels />}
          {tab === "submit" && <Submit />}
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
