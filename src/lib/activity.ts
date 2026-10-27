import type { FleetActivity } from "../bindings/FleetActivity";
import type { FleetAnalyticsWindow } from "../bindings/FleetAnalyticsWindow";
import type { FleetCompletedJob } from "../bindings/FleetCompletedJob";
import type { FleetJobConclusion } from "../bindings/FleetJobConclusion";
import type { FleetJobOs } from "../bindings/FleetJobOs";
import type { Tone } from "./status";

export const WINDOWS: { value: FleetAnalyticsWindow; label: string; long: string }[] = [
  { value: "24h", label: "24 hours", long: "the last 24 hours" },
  { value: "7d", label: "7 days", long: "the last 7 days" },
  { value: "30d", label: "30 days", long: "the last 30 days" },
];

export const conclusionStyle: Record<FleetJobConclusion, { label: string; tone: Tone }> = {
  success: { label: "Succeeded", tone: "ok" },
  failure: { label: "Failed", tone: "bad" },
  timedOut: { label: "Timed out", tone: "bad" },
  startupFailure: { label: "Failed to start", tone: "bad" },
  cancelled: { label: "Cancelled", tone: "muted" },
  skipped: { label: "Skipped", tone: "muted" },
  neutral: { label: "Neutral", tone: "muted" },
  stale: { label: "Stale", tone: "muted" },
  actionRequired: { label: "Action required", tone: "warn" },
  unknown: { label: "Unknown", tone: "muted" },
};

export const osLabel: Record<FleetJobOs, string> = { linux: "Linux", windows: "Windows", macos: "macOS" };

/** A duration for humans: `45s`, `3m 20s`, `1h 05m`, `2d 3h`. */
export function formatDuration(secs: number | null | undefined): string {
  if (secs == null) return "—";
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return s % 60 ? `${m}m ${s % 60}s` : `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${String(m % 60).padStart(2, "0")}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** Runner time in hours, for totals: `0.4 h`, `12 h`, `1,204 h`. */
export function formatHours(secs: number): string {
  const h = secs / 3600;
  return `${h < 10 ? h.toFixed(1) : Math.round(h).toLocaleString("en-US")} h`;
}

export function formatUsd(usd: number): string {
  return usd >= 1000
    ? `$${Math.round(usd).toLocaleString("en-US")}`
    : `$${usd.toFixed(2)}`;
}

/** A busy or success share: `12%`, `<1%` for small non-zero values, `—` when unknown. */
export function formatShare(percent: number | null | undefined): string {
  if (percent == null) return "—";
  if (percent > 0 && percent < 1) return "<1%";
  return `${Math.round(percent)}%`;
}

/** Recent completed jobs for one runner or machine, newest first. */
export function recentJobs(activity: FleetActivity, by: { runnerKey?: string; machineKey?: string }, limit = 10): FleetCompletedJob[] {
  return activity.recent
    .filter((j) => (by.runnerKey ? j.runnerKey === by.runnerKey : true) && (by.machineKey ? j.machineKey === by.machineKey : true))
    .slice(0, limit);
}

/** Bar heights for a chart, as fractions of the tallest value. */
export function barFractions(values: number[]): number[] {
  const max = Math.max(0, ...values);
  return values.map((v) => (max > 0 ? v / max : 0));
}

/** The user's rates as a form, and the parsed values when they're all valid. */
export function parseRate(text: string): number | null {
  const value = Number(text.trim().replace(/^\$/, ""));
  return text.trim() !== "" && Number.isFinite(value) && value >= 0 && value <= 10 ? value : null;
}
