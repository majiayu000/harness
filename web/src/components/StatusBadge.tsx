interface Props {
  ok: boolean;
}

export function StatusBadge({ ok }: Props) {
  return (
    <div
      className={`inline-flex items-center gap-2 px-2.5 py-1 rounded-[3px] font-mono text-[11px] border ${
        ok
          ? "text-ok border-ok/40"
          : "text-danger border-danger/40"
      }`}
    >
      <span
        className={`w-[7px] h-[7px] rounded-full ${ok ? "bg-ok" : "bg-danger"}`}
      />
      {ok ? "all systems nominal" : "connection lost"}
    </div>
  );
}
