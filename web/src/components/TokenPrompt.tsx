import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { TOKEN_KEY, unauthorizedEvents } from "@/lib/api";

export function TokenPrompt() {
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState("");
  const inputRef = useRef<HTMLInputElement | null>(null);
  const qc = useQueryClient();

  useEffect(() => {
    const onUnauthorized = () => setOpen(true);
    unauthorizedEvents.addEventListener("unauthorized", onUnauthorized);
    return () => unauthorizedEvents.removeEventListener("unauthorized", onUnauthorized);
  }, []);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  if (!open) return null;

  const save = () => {
    const v = value.trim();
    if (!v) return;
    try {
      sessionStorage.setItem(TOKEN_KEY, v);
    } catch {
      // ignore
    }
    setOpen(false);
    setValue("");
    qc.invalidateQueries();
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-[9999]">
      <div className="bg-bg-1 p-6 border border-line-2 min-w-[320px] font-sans">
        <p className="m-0 mb-4 font-semibold text-ink">Enter API token</p>
        <input
          ref={inputRef}
          type="password"
          placeholder="Bearer token"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") save();
          }}
          className="w-full p-2 border border-line-2 bg-bg text-ink font-mono box-border"
        />
        <div className="mt-4 flex gap-2 justify-end">
          <button onClick={save} className="px-3.5 py-1.5 bg-rust text-white border-0 cursor-pointer font-mono">
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
