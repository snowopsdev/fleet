import type { FleetEventKind } from "../bindings/FleetEventKind";
import type { FleetMachineEvent } from "../bindings/FleetMachineEvent";
import type { FleetView } from "../bindings/FleetView";
import { allMachines } from "./nav";
import type { Tone } from "./status";

export const eventStyle: Record<FleetEventKind, { label: string; tone: Tone }> = {
  monitoringStarted: { label: "Monitoring started", tone: "muted" },
  targetChanged: { label: "Target changed", tone: "info" },
  online: { label: "Back online", tone: "ok" },
  offline: { label: "Went offline", tone: "muted" },
  sshUnavailable: { label: "SSH unavailable", tone: "warn" },
  sshRecovered: { label: "SSH recovered", tone: "ok" },
  rebooted: { label: "Reboot observed", tone: "info" },
  runnerServiceChanged: { label: "Runner service changed", tone: "info" },
  offlineAlert: { label: "Offline alert", tone: "warn" },
  lowDisk: { label: "Low disk space", tone: "warn" },
  diskRecovered: { label: "Disk space recovered", tone: "ok" },
  maintenanceStarted: { label: "Maintenance started", tone: "muted" },
  maintenanceEnded: { label: "Maintenance ended", tone: "muted" },
  alertsResumed: { label: "Alert after maintenance", tone: "warn" },
  drainRequested: { label: "Draining", tone: "info" },
  drained: { label: "Drained", tone: "info" },
  drainCancelled: { label: "Drain ended", tone: "muted" },
  resumed: { label: "Resumed", tone: "ok" },
  watchdogRestarted: { label: "Restarted automatically", tone: "info" },
  watchdogRestartFailed: { label: "Automatic restart failed", tone: "warn" },
  watchdogGaveUp: { label: "Automatic restarts stopped", tone: "bad" },
  watchdogRecovered: { label: "Running again", tone: "ok" },
  wakeRequested: { label: "Wake requested", tone: "info" },
};

/** History pages hold this many events: `PAGE_SIZE` in crates/fleet/src/monitoring/store.rs. */
export const HISTORY_PAGE_SIZE = 50;

/** Inventory keys survive a tailnet node replacement; selection uses the current UI ID. */
export function monitoringMachine(view: FleetView, key: string) {
  return allMachines(view).find((m) => m.monitoring.machineKey === key);
}

/** Cursor IDs are fixed-width decimal strings, so they sort without numeric precision loss. */
export function mergeEvents(first: FleetMachineEvent[], second: FleetMachineEvent[]): FleetMachineEvent[] {
  return [...new Map([...second, ...first].map((e) => [e.id, e])).values()]
    .sort((a, b) => b.id.localeCompare(a.id));
}
