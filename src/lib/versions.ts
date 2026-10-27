import type { FleetView } from "../bindings/FleetView";
import type { RunnerView } from "../bindings/RunnerView";
import { allRunners } from "./nav";

/** Runners behind the latest `actions/runner` release, overdue ones first. */
export function runnersBehind(view: FleetView): RunnerView[] {
  return allRunners(view)
    .filter((r) => r.versionStatus?.updateAvailable)
    .sort((a, b) => Number(b.versionStatus!.overdue) - Number(a.versionStatus!.overdue) || a.name.localeCompare(b.name));
}

/** A runner's version for display, with the available update: `2.328.0 · 2.329.0 available`. */
export function versionText(r: RunnerView, version: string | null | undefined): string {
  const status = r.versionStatus;
  const current = version ?? status?.current;
  if (!current) return "—";
  if (!status?.updateAvailable) return status ? `${current} (latest)` : current;
  return `${current} · ${status.latest} available${status.overdue ? ", update overdue" : ""}`;
}

/** One line about runner versions for the overview, or null before the latest release is known. */
export function releaseSummary(view: FleetView): string | null {
  const release = view.runnerRelease;
  if (!release) return null;
  const behind = runnersBehind(view);
  const overdue = behind.filter((r) => r.versionStatus?.overdue).length;
  if (behind.length === 0) return `Runners are on the latest release, ${release.version}.`;
  const count = behind.length === 1 ? "1 runner is" : `${behind.length} runners are`;
  return `${count} behind runner ${release.version}${overdue ? ` (${overdue} overdue)` : ""}.`;
}
