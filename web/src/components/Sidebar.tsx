import { Link } from "react-router-dom";

export interface SidebarItem {
  id: string;
  label: string;
  href?: string;
  icon?: React.ReactNode;
  count?: number;
  active?: boolean;
}

export interface SidebarSection {
  label: string;
  items: SidebarItem[];
}

interface Props {
  env: string;
  sections: SidebarSection[];
  userInitials?: string;
  userName?: string;
  userEmail?: string;
  onItemClick?: (id: string) => void;
}

function isExternalHref(href: string | undefined): boolean {
  return href?.startsWith("http://") === true || href?.startsWith("https://") === true;
}

export function Sidebar({
  env,
  sections,
  userInitials = "MJ",
  userName = "majiayu",
  userEmail = "mj@harness.local",
  onItemClick,
}: Props) {
  return (
    <aside className="border-r border-line bg-bg-1 flex flex-col min-h-0 w-[240px]">
      <div className="px-[18px] py-4 flex items-center gap-2.5 border-b border-line">
        <span className="w-[22px] h-[22px] text-rust">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" className="w-[22px] h-[22px]">
            <path d="M4 4 L12 8 L20 4 L20 20 L12 16 L4 20 Z" fill="color-mix(in oklab, currentColor 18%, transparent)" />
            <path d="M12 8 L12 16" />
          </svg>
        </span>
        <span className="font-mono text-[13px] font-semibold tracking-[0.02em]">harness</span>
        <span className="ml-auto font-mono text-[10.5px] text-ink-3 px-[7px] py-0.5 border border-line-2 rounded-[2px]">
          {env}
        </span>
      </div>

      <nav className="px-2 py-2.5 flex-1 overflow-auto">
        {sections.map((section) => (
          <div key={section.label}>
            <div className="font-mono text-[10px] tracking-[0.14em] uppercase text-ink-4 px-2.5 pt-3.5 pb-1.5">
              {section.label}
            </div>
            {section.items.map((item) => {
              const itemClass = `flex items-center gap-2.5 px-2.5 py-[7px] rounded-[4px] text-[13px] cursor-pointer relative ${
                item.active
                  ? "bg-bg-2 text-ink before:content-[''] before:absolute before:-left-2 before:top-2 before:bottom-2 before:w-0.5 before:bg-rust"
                  : "text-ink-2 hover:bg-bg-2 hover:text-ink"
              }`;
              const body = (
                <>
                  {item.icon && <span className="w-[15px] h-[15px] text-ink-3 flex-none">{item.icon}</span>}
                  {item.label}
                  {typeof item.count === "number" && (
                    <span
                      className={`ml-auto font-mono text-[10.5px] px-1.5 py-[1px] border rounded-[10px] min-w-[22px] text-center ${
                        item.active ? "text-rust border-rust" : "text-ink-3 border-line-2"
                      }`}
                    >
                      {item.count}
                    </span>
                  )}
                </>
              );

              if (!item.href && onItemClick) {
                return (
                  <button
                    key={item.id}
                    type="button"
                    className={`${itemClass} w-full text-left bg-transparent border-0`}
                    onClick={() => onItemClick(item.id)}
                  >
                    {body}
                  </button>
                );
              }

              if (isExternalHref(item.href)) {
                return (
                  <a key={item.id} href={item.href} target="_blank" rel="noreferrer" className={itemClass}>
                    {body}
                  </a>
                );
              }

              return (
                <Link key={item.id} to={item.href!} className={itemClass}>
                  {body}
                </Link>
              );
            })}
          </div>
        ))}
      </nav>

      <div className="px-[14px] py-3 border-t border-line flex gap-2.5 items-center">
        <div className="w-[26px] h-[26px] rounded-full grid place-items-center text-white font-mono text-[11px] font-semibold bg-gradient-to-br from-rust to-rust-deep">
          {userInitials}
        </div>
        <div className="flex-1 min-w-0">
          <div className="text-[12.5px] text-ink">{userName}</div>
          <div className="font-mono text-[10.5px] text-ink-3 overflow-hidden text-ellipsis whitespace-nowrap">{userEmail}</div>
        </div>
      </div>
    </aside>
  );
}
