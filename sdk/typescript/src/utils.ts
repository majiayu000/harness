import type { TurnSnapshot } from "./types";

export function normalizeBaseUrl(value: string | undefined): string {
  return (value ?? "http://127.0.0.1:9800").replace(/\/+$/, "");
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function isTerminalStatus(status: string): boolean {
  return status === "completed" || status === "cancelled" || status === "failed";
}

export function turnSignature(turn: TurnSnapshot): string {
  const itemCount = Array.isArray(turn.items) ? turn.items.length : 0;
  return `${turn.status}|${itemCount}|${turn.completed_at ?? ""}`;
}

function pushCandidate(messages: string[], candidate: unknown): boolean {
  if (typeof candidate === "string" && candidate.trim()) {
    messages.push(candidate.trim());
    return true;
  }
  return false;
}

export function extractOutput(turn: TurnSnapshot | undefined): string {
  if (!turn || !Array.isArray(turn.items)) {
    return "";
  }

  const messages: string[] = [];
  for (const item of turn.items) {
    if (item.type === "user_message") {
      continue;
    }

    if (pushCandidate(messages, (item as { content?: unknown }).content)) {
      continue;
    }

    if (pushCandidate(messages, (item as { stdout?: unknown }).stdout)) {
      continue;
    }

    pushCandidate(messages, (item as { message?: unknown }).message);
  }

  return messages.join("\n\n");
}

export async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  timeoutMessage: string,
): Promise<T> {
  let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
  const timeoutPromise = new Promise<T>((_, reject) => {
    timeoutHandle = setTimeout(() => reject(new Error(timeoutMessage)), timeoutMs);
  });

  try {
    return await Promise.race([promise, timeoutPromise]);
  } finally {
    if (timeoutHandle) {
      clearTimeout(timeoutHandle);
    }
  }
}
