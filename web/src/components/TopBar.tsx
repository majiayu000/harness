import type { ReactNode } from "react";

export interface Crumb {
  label: string;
  href?: string;
  current?: boolean;
}

interface Props {
  breadcrumb: Crumb[];
  searchPlaceholder?: string;
  actions?: ReactNode;
}

export function TopBar({ breadcrumb, searchPlaceholder = "Search…", actions }: Props) {
  return (
    <div className="h-12 border-b border-line bg-bg flex items-center px-[18px] gap-3 flex-none">
      <div className="font-mono text-[12px] text-ink-3 flex items-center gap-2">
        {breadcrumb.map((c, i) => (
          <span key={i} className="flex items-center gap-2">
            {i > 0 && <span className="text-ink-4">/</span>}
            {c.current ? (
              <b className="text-ink font-medium">{c.label}</b>
            ) : c.href ? (
              <a href={c.href}>{c.label}</a>
            ) : (
              <span>{c.label}</span>
            )}
          </span>
        ))}
      </div>
      <div className="flex-1" />
      <div className="relative w-[320px]">
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.6"
          className="absolute left-2.5 top-[7px] w-3.5 h-3.5 text-ink-3"
        >
          <circle cx="11" cy="11" r="7" />
          <path d="M16 16l5 5" />
        </svg>
        <input
          placeholder={searchPlaceholder}
          className="w-full h-[30px] bg-bg-1 border border-line-2 px-2.5 pl-8 text-ink font-mono text-[12px] rounded-[3px] outline-none focus:border-rust placeholder:text-ink-3"
        />
        <span className="absolute right-2 top-1.5 font-mono text-[10.5px] px-1.5 py-0.5 border border-line-2 text-ink-3 rounded-[3px]">
          ⌘K
        </span>
      </div>
      {actions}
    </div>
  );
}
