import { describe, expect, it } from "vitest";
import { fmtInt, fmtScore, fmtPct, formatDurationShort, relativeAgo } from "./format";

describe("fmtInt", () => {
  it("formats integers with thousands separators", () => {
    expect(fmtInt(1234567)).toBe("1,234,567");
    expect(fmtInt(0)).toBe("0");
  });
  it("returns em-dash for null / undefined / NaN", () => {
    expect(fmtInt(null)).toBe("—");
    expect(fmtInt(undefined)).toBe("—");
    expect(fmtInt(Number.NaN)).toBe("—");
  });
});

describe("fmtScore", () => {
  it("rounds to integer", () => {
    expect(fmtScore(91.6)).toBe("92");
    expect(fmtScore(91.4)).toBe("91");
  });
  it("em-dashes missing", () => {
    expect(fmtScore(null)).toBe("—");
  });
});

describe("fmtPct", () => {
  it("formats to one decimal", () => {
    expect(fmtPct(2.14)).toBe("2.1");
    expect(fmtPct(0)).toBe("0.0");
  });
  it("em-dashes missing", () => {
    expect(fmtPct(null)).toBe("—");
  });
});

describe("formatDurationShort", () => {
  it("formats seconds, minutes, hours, and days", () => {
    expect(formatDurationShort(45)).toBe("45s");
    expect(formatDurationShort(120)).toBe("2m");
    expect(formatDurationShort(10_800)).toBe("3h");
    expect(formatDurationShort(172_800)).toBe("2d");
  });

  it("returns em-dash for missing values", () => {
    expect(formatDurationShort(null)).toBe("—");
  });
});

describe("relativeAgo", () => {
  const now = new Date("2026-04-19T12:00:00Z");
  it("formats seconds", () => {
    expect(relativeAgo(new Date("2026-04-19T11:59:46Z"), now)).toBe("14s");
  });
  it("formats minutes", () => {
    expect(relativeAgo(new Date("2026-04-19T11:55:00Z"), now)).toBe("5m");
  });
  it("formats hours", () => {
    expect(relativeAgo(new Date("2026-04-19T09:00:00Z"), now)).toBe("3h");
  });
  it("formats days", () => {
    expect(relativeAgo(new Date("2026-04-17T12:00:00Z"), now)).toBe("2d");
  });
  it("clamps future dates to 0s", () => {
    expect(relativeAgo(new Date("2026-04-19T13:00:00Z"), now)).toBe("0s");
  });
});
