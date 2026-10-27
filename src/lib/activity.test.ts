import { describe, expect, it } from "vitest";
import { barFractions, formatDuration, formatHours, formatShare, formatUsd, parseRate, recentJobs } from "./activity";
import { previewActivity, previewFleet } from "./preview";

describe("activity formatting", () => {
  it("formats durations, hours, money and shares compactly", () => {
    expect([null, 0, 45, 200, 3600 + 5 * 60, 50 * 3600].map(formatDuration)).toEqual(["—", "0s", "45s", "3m 20s", "1h 05m", "2d 2h"]);
    expect([1440, 36000, 4_334_400].map(formatHours)).toEqual(["0.4 h", "10 h", "1,204 h"]);
    expect([0.5, 12.346, 1234.5].map(formatUsd)).toEqual(["$0.50", "$12.35", "$1,235"]);
    expect([null, 0, 0.4, 12.6, 100].map(formatShare)).toEqual(["—", "0%", "<1%", "13%", "100%"]);
  });

  it("scales bars to the tallest value and validates rates", () => {
    expect(barFractions([0, 5, 10])).toEqual([0, 0.5, 1]);
    expect(barFractions([0, 0])).toEqual([0, 0]);
    expect(["0.006", "$0.062", "", "-1", "11", "abc"].map(parseRate)).toEqual([0.006, 0.062, null, null, null, null]);
  });
});

describe("demo activity", () => {
  const view = previewFleet();
  it("gives each runner pane its own recent jobs", () => {
    const studio1 = recentJobs(view.activity, { runnerKey: "gh:acme-demo:101" });
    expect(studio1.length).toBeGreaterThan(0);
    expect(studio1.every((j) => j.runnerKey === "gh:acme-demo:101")).toBe(true);
    const studio = recentJobs(view.activity, { machineKey: "inv:studio" });
    expect(studio.every((j) => j.machineKey === "inv:studio")).toBe(true);
    expect(studio.map((j) => j.completedAt)).toEqual([...studio.map((j) => j.completedAt)].sort().reverse());
  });

  it("shifts the activity fixture to the same time as the fleet fixture", () => {
    const activity = previewActivity();
    const day = activity.analytics["24h"];
    expect(Math.abs(Date.parse(day.to) - Date.parse(view.generatedAt))).toBeLessThan(60_000);
    expect(day.buckets).toHaveLength(24);
    expect(activity.history.jobs.length).toBeGreaterThan(0);
  });
});
