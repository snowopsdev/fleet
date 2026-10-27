/**
 * Typed wrappers over Tauri commands and events.
 *
 * Outside Tauri (plain `pnpm dev` in a browser) everything is served from the
 * demo fixture so layouts can be iterated on; the view is marked `demo`.
 */
import type { AppConfig } from "../bindings/AppConfig";
import type { FleetView } from "../bindings/FleetView";
import type { FleetMachineEvent } from "../bindings/FleetMachineEvent";
import type { ChangeRequest } from "../bindings/ChangeRequest";
import type { ChangeSecrets } from "../bindings/ChangeSecrets";
import type { ReviewView } from "../bindings/ReviewView";
import type { FleetWakeReview } from "../bindings/FleetWakeReview";
import type { GhStatus } from "../bindings/GhStatus";
import type { FleetAlertDestinationInput } from "../bindings/FleetAlertDestinationInput";
import type { FleetPublishSettings } from "../bindings/FleetPublishSettings";
import type { AccountKind } from "../bindings/AccountKind";
import type { OrgAuth } from "../bindings/OrgAuth";
import type { RepoScope } from "../bindings/RepoScope";
import type { FleetAnalytics } from "../bindings/FleetAnalytics";
import type { FleetAnalyticsWindow } from "../bindings/FleetAnalyticsWindow";
import type { FleetHostedRates } from "../bindings/FleetHostedRates";
import type { FleetJobHistoryFilter } from "../bindings/FleetJobHistoryFilter";
import type { FleetJobHistoryPage } from "../bindings/FleetJobHistoryPage";
import { previewActivity, previewFleet } from "./preview";

const previewConfigEvents = new EventTarget();

export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Live data in the desktop app, where commands can act on it. */
export function isLive(view: FleetView): boolean {
  return isTauri && view.source === "live";
}

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri) throw new Error(`"${cmd}" is only available in the desktop app`);
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

/** Notify every window only after a settings mutation succeeds. */
async function updateConfig(cmd: string, args?: Record<string, unknown>): Promise<void> {
  await invoke<void>(cmd, args);
  // A notification failure must not turn a completed save into a reported failure.
  await emitEvent("config-updated").catch((error) => console.error("Couldn't notify settings listeners", error));
}

export const commands = {
  getFleet: (): Promise<FleetView> => (isTauri ? invoke("get_fleet") : Promise.resolve(previewFleet())),
  getConfig: (): Promise<AppConfig> =>
    isTauri ? invoke("get_config") : Promise.resolve({ orgs: [], ignoredPeers: [], machinePreferences: {}, demoMode: true, localApi: false, desktopNotifications: false, inventoryPath: null, automation: { watchdog: false, alertDestinations: [], publish: { enabled: false, variable: "RUNNER_FLEET_STATUS", intervalMinutes: 5 } }, jobActivity: { hostedRates: { linux: 0.006, windows: 0.01, macos: 0.062, asOf: "2026-10-26" }, feedOrgs: [] }, sshConnectionReuse: false }),
  refreshNow: () => invoke<void>("refresh_now"),
  addOrg: (login: string, auth: OrgAuth, token: string | null, repoScope: RepoScope, kind: AccountKind = "organization", runnerRepos: string[] = []) =>
    updateConfig("add_org", { login, auth, token, repoScope, kind, runnerRepos }),
  getGhStatus: (): Promise<GhStatus> =>
    isTauri
      ? invoke("get_gh_status")
      : Promise.resolve({ installed: false, path: null, host: "github.com", login: null, scopes: [], missingRunnersScope: false, problem: "Only in the desktop app", fix: null }),
  useGhForOrg: (login: string) => updateConfig("use_gh_for_org", { login }),
  updateOrgToken: (login: string, token: string) => updateConfig("update_org_token", { login, token }),
  setOrgScope: (login: string, repoScope: RepoScope) => updateConfig("set_org_scope", { login, repoScope }),
  setOrgRunnerRepos: (login: string, runnerRepos: string[]) => updateConfig("set_org_runner_repos", { login, runnerRepos }),
  removeOrg: (login: string) => updateConfig("remove_org", { login }),
  ignorePeer: (nodeId: string) => updateConfig("ignore_peer", { nodeId }),
  unignorePeer: (nodeId: string) => updateConfig("unignore_peer", { nodeId }),
  setMachinePreferences: (id: string, displayName: string, description: string) =>
    updateConfig("set_machine_preferences", { id, displayName, description }),
  setDemoMode: (on: boolean) => updateConfig("set_demo_mode", { on }),
  setMachineMaintenance: (id: string, paused: boolean) => invoke<void>("set_machine_maintenance", { id, paused }),
  getMonitoringEvents: (machineKey: string | null, before: string | null) =>
    invoke<FleetMachineEvent[]>("get_monitoring_events", { machineKey, before }),
  setDesktopNotifications: (enabled: boolean) => updateConfig("set_desktop_notifications", { enabled }),
  testDesktopNotification: () => invoke<void>("test_desktop_notification"),
  openGithubUrl: (url: string) => (isTauri ? invoke<void>("open_github_url", { url }) : Promise.resolve(void window.open(url))),

  // Desired state (inventory/fleet.toml). Checks only read; changes run from a
  // review, and only if the host still matches it.
  desiredRefresh: (host: string | null = null) => invoke<void>("desired_refresh", { host }),
  desiredDoctor: (host: string | null = null) => invoke<void>("desired_doctor", { host }),
  desiredLogs: (runner: string, lines: number) => invoke<string>("desired_logs", { runner, lines }),
  desiredReview: (request: ChangeRequest) => invoke<ReviewView>("desired_review", { request }),
  desiredExecute: (reviewId: string, secrets: ChangeSecrets, confirmation: string | null) =>
    invoke<void>("desired_execute", { reviewId, secrets, confirmation }),
  desiredDismissOperation: () => invoke<void>("desired_dismiss_operation"),
  /** Cancels a reviewed drain that hasn't stopped its runner yet. */
  desiredCancelDrain: (runner: string) => invoke<void>("desired_cancel_drain", { runner }),

  // Automatic behaviour. Turning the watchdog on is the standing approval for its restarts.
  setWatchdog: (enabled: boolean) => updateConfig("set_watchdog", { enabled }),
  setPublishSettings: (settings: FleetPublishSettings) => updateConfig("set_publish_settings", { settings }),
  /** Webhook URLs and tokens go to the Keychain and are never returned. */
  saveAlertDestination: (destination: FleetAlertDestinationInput) => invoke<string>("save_alert_destination", { destination }),
  removeAlertDestination: (id: string) => invoke<void>("remove_alert_destination", { id }),
  testAlertDestination: (id: string) => invoke<void>("test_alert_destination", { id }),
  // Wake-on-LAN: reviewed first, then sent once from the machine's wake_via host.
  wakeReview: (id: string) => invoke<FleetWakeReview>("wake_review", { id }),
  wakeExecute: (reviewId: string) => invoke<string>("wake_execute", { reviewId }),

  // Completed-job history and analytics (local data only), and the optional job feed.
  getJobHistory: (filter: FleetJobHistoryFilter): Promise<FleetJobHistoryPage> =>
    isTauri ? invoke("get_job_history", { filter }) : Promise.resolve(previewHistory(filter)),
  getAnalytics: (window: FleetAnalyticsWindow): Promise<FleetAnalytics> =>
    isTauri ? invoke("get_analytics", { window }) : Promise.resolve(previewActivity().analytics[window]),
  setHostedRates: (rates: FleetHostedRates) => updateConfig("set_hosted_rates", { rates }),
  setJobFeed: (org: string, enabled: boolean) => updateConfig("set_job_feed", { org, enabled }),

  // Local read-only API for agents
  getLocalApiStatus: (): Promise<LocalApiStatus> =>
    isTauri
      ? invoke("get_local_api_status")
      : Promise.resolve({ enabled: false, url: null, error: "Only in the desktop app", discoveryFile: null }),
  setLocalApi: (on: boolean) => updateConfig("set_local_api", { on }),
  setSshConnectionReuse: (on: boolean) => updateConfig("set_ssh_connection_reuse", { on }),
};

/** The preview fixture's first history page, filtered like the desktop app's first page. */
function previewHistory(filter: FleetJobHistoryFilter): FleetJobHistoryPage {
  const page = previewActivity().history;
  if (filter.before || filter.after) return { ...page, jobs: [], hasMore: false, nextCursor: null };
  const jobs = page.jobs.filter((j) =>
    (!filter.runnerKey || j.runnerKey === filter.runnerKey)
    && (!filter.machineKey || j.machineKey === filter.machineKey)
    && (!filter.conclusion || j.conclusion === filter.conclusion));
  return { ...page, jobs: jobs.slice(0, filter.limit ?? 50), hasMore: false, nextCursor: null };
}

/** Mirrors `LocalApiStatus` in src-tauri/src/local_api.rs, which has no generated binding. */
export interface LocalApiStatus {
  enabled: boolean;
  url: string | null;
  error: string | null;
  discoveryFile: string | null;
}

export async function onFleetUpdated(handler: (view: FleetView) => void): Promise<() => void> {
  if (!isTauri) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<FleetView>("fleet-updated", (e) => handler(e.payload));
}

export async function onConfigUpdated(handler: () => void): Promise<() => void> {
  if (!isTauri) {
    previewConfigEvents.addEventListener("config-updated", handler);
    return () => previewConfigEvents.removeEventListener("config-updated", handler);
  }
  const { listen } = await import("@tauri-apps/api/event");
  return listen("config-updated", handler);
}

export async function emitEvent(name: string): Promise<void> {
  if (!isTauri) {
    if (name === "config-updated") previewConfigEvents.dispatchEvent(new Event(name));
    return;
  }
  const { emit } = await import("@tauri-apps/api/event");
  await emit(name);
}

export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
