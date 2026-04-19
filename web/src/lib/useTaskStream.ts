import { useEffect, useState } from "react";
import { fetchEventSource } from "@microsoft/fetch-event-source";
import { authHeaders, unauthorizedEvents } from "./api";

const MAX_LINES = 2000;
const MAX_EVENT_BYTES = 4096;

export interface TaskStreamState {
  lines: string[];
  connected: boolean;
  done: boolean;
  error: string | null;
}

export function useTaskStream(taskId: string | null): TaskStreamState {
  const [lines, setLines] = useState<string[]>([]);
  const [connected, setConnected] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!taskId) {
      setLines([]);
      setConnected(false);
      setDone(false);
      setError(null);
      return;
    }

    const controller = new AbortController();

    setLines([]);
    setConnected(false);
    setDone(false);
    setError(null);

    fetchEventSource(`/tasks/${taskId}/stream`, {
      headers: { ...authHeaders(), Accept: "text/event-stream" },
      signal: controller.signal,
      openWhenHidden: true,
      onopen: async (resp) => {
        if (resp.status === 401) {
          unauthorizedEvents.dispatchEvent(new Event("unauthorized"));
          controller.abort();
          return;
        }
        if (!resp.ok) {
          setError(`Stream request failed: ${resp.status}`);
          setConnected(false);
          controller.abort();
          return;
        }
        const ct = resp.headers.get("content-type") ?? "";
        if (!ct.includes("text/event-stream")) {
          setError(`Unexpected response type (status ${resp.status})`);
          setConnected(false);
          controller.abort();
          return;
        }
        setConnected(true);
      },
      onmessage: (ev) => {
        if (!ev.data) return;
        const data =
          ev.data.length > MAX_EVENT_BYTES
            ? ev.data.slice(0, MAX_EVENT_BYTES) + " …[truncated]"
            : ev.data;
        setLines((prev) => {
          const next = [...prev, data];
          return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
        });
      },
      onerror: (err) => {
        setError(String(err));
        setConnected(false);
        // Rethrow to stop automatic retry.
        throw err;
      },
      onclose: () => {
        setConnected(false);
        setDone(true);
      },
    }).catch(() => {
      // onerror rethrows to stop retry; catch here to avoid unhandled rejection.
    });

    return () => {
      controller.abort();
    };
  }, [taskId]);

  return { lines, connected, done, error };
}
