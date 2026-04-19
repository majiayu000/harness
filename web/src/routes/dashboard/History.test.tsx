import { describe, expect, it } from "vitest";
import type { Task } from "@/types";
import { filterAndPage } from "./History";

function makeTask(overrides: Partial<Task> & { id: string; status: string }): Task {
  return {
    turn: 0,
    pr_url: null,
    error: null,
    source: null,
    parent_id: null,
    repo: null,
    description: null,
    created_at: null,
    phase: null,
    depends_on: [],
    subtask_ids: [],
    project: null,
    ...overrides,
  };
}

const tasks: Task[] = [
  makeTask({ id: "1", status: "done", description: "fix login bug", repo: "acme/api", created_at: "2026-04-18T10:00:00Z" }),
  makeTask({ id: "2", status: "failed", description: "deploy infra", repo: "acme/ops", created_at: "2026-04-19T08:00:00Z" }),
  makeTask({ id: "3", status: "cancelled", description: "refactor db", repo: "acme/db", created_at: "2026-04-17T06:00:00Z" }),
  makeTask({ id: "4", status: "implementing", description: "active task", repo: "acme/web", created_at: "2026-04-20T01:00:00Z" }),
  makeTask({ id: "5", status: "done", description: "update docs", repo: "acme/docs", pr_url: "https://github.com/acme/docs/pull/7", created_at: "2026-04-20T00:00:00Z" }),
];

describe("filterAndPage", () => {
  it("excludes non-terminal tasks", () => {
    const { rows } = filterAndPage(tasks, "all", "", 0);
    expect(rows.every((r) => ["done", "failed", "cancelled"].includes(r.status))).toBe(true);
    expect(rows.find((r) => r.id === "4")).toBeUndefined();
  });

  it("sorts by created_at descending", () => {
    const { rows } = filterAndPage(tasks, "all", "", 0);
    const dates = rows.map((r) => new Date(r.created_at!).getTime());
    expect(dates).toEqual([...dates].sort((a, b) => b - a));
  });

  it("filters by done status", () => {
    const { rows } = filterAndPage(tasks, "done", "", 0);
    expect(rows.every((r) => r.status === "done")).toBe(true);
    expect(rows.length).toBe(2);
  });

  it("filters by failed status", () => {
    const { rows } = filterAndPage(tasks, "failed", "", 0);
    expect(rows.every((r) => r.status === "failed")).toBe(true);
    expect(rows.length).toBe(1);
  });

  it("search query filters by description", () => {
    const { rows } = filterAndPage(tasks, "all", "login", 0);
    expect(rows.length).toBe(1);
    expect(rows[0].id).toBe("1");
  });

  it("search query filters by repo", () => {
    const { rows } = filterAndPage(tasks, "all", "acme/ops", 0);
    expect(rows.length).toBe(1);
    expect(rows[0].id).toBe("2");
  });

  it("search query filters by pr_url", () => {
    const { rows } = filterAndPage(tasks, "all", "pull/7", 0);
    expect(rows.length).toBe(1);
    expect(rows[0].id).toBe("5");
  });

  it("paginates correctly — page 0", () => {
    const bigTasks = Array.from({ length: 25 }, (_, i) =>
      makeTask({ id: `t${i}`, status: "done", created_at: `2026-04-${String(i + 1).padStart(2, "0")}T00:00:00Z` }),
    );
    const { rows, totalPages } = filterAndPage(bigTasks, "all", "", 0);
    expect(totalPages).toBe(2);
    expect(rows.length).toBe(20);
  });

  it("paginates correctly — page 1", () => {
    const bigTasks = Array.from({ length: 25 }, (_, i) =>
      makeTask({ id: `t${i}`, status: "done", created_at: `2026-04-${String(i + 1).padStart(2, "0")}T00:00:00Z` }),
    );
    const { rows, totalPages } = filterAndPage(bigTasks, "all", "", 1);
    expect(totalPages).toBe(2);
    expect(rows.length).toBe(5);
  });

  it("clamps page to last valid page when out of range", () => {
    const { rows } = filterAndPage(tasks, "all", "", 99);
    expect(rows.length).toBeGreaterThan(0);
  });

  it("returns totalPages=1 for empty results", () => {
    const { totalPages, rows } = filterAndPage([], "all", "", 0);
    expect(totalPages).toBe(1);
    expect(rows.length).toBe(0);
  });
});
