import { useEffect, useState } from "react";
import { fetchEventSource } from "@microsoft/fetch-event-source";
import { authHeaders, unauthorizedEvents } from "./api";

const MAX_LINES = 2000;

export interface TaskStreamState {
  lines: string[];
  connected: boolean;
  error: string | null;
}

export function useTaskStream(taskId: string | null): TaskStreamState {
  const [lines, setLines] = useState<string[]>([]);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!taskId) {
      setLines([]);
      setConnected(false);
      setError(null);
      return;
    }

    const controller = new AbortController();

    setLines([]);
    setConnected(false);
    setError(null);

    fetchEventSource(`/tasks/${taskId}/stream`, {
      headers: { ...authHeaders(), Accept: "text/event-stream" },
      signal: controller.signal,
      onopen: async (resp) => {
        if (resp.status === 401) {
          unauthorizedEvents.dispatchEvent(new Event("unauthorized"));
          controller.abort();
          return;
        }
        setConnected(true);
      },
      onmessage: (ev) => {
        if (!ev.data) return;
        setLines((prev) => {
          const next = [...prev, ev.data];
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
      },
    });

    return () => {
      controller.abort();
    };
  }, [taskId]);

  return { lines, connected, error };
}
