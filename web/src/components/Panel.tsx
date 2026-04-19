import type { ReactNode } from "react";

interface Props {
  title: string;
  sub?: string;
  right?: ReactNode;
  children: ReactNode;
  className?: string;
  id?: string;
}

export function Panel({ title, sub, right, children, className = "", id }: Props) {
  return (
    <div id={id} className={`border-b border-line ${className}`}>
      <div className="px-[18px] py-2.5 border-b border-line flex items-center gap-2.5 bg-bg sticky top-0 z-[1]">
        <h3 className="m-0 font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 font-medium">{title}</h3>
        {sub && <span className="font-mono text-[11px] text-ink-4 ml-0.5">{sub}</span>}
        {right && <div className="ml-auto font-mono text-[11px] text-ink-3 flex items-center gap-2.5">{right}</div>}
      </div>
      {children}
    </div>
  );
}
