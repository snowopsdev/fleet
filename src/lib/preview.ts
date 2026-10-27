import type { FleetAnalytics } from "../bindings/FleetAnalytics";
import type { FleetAnalyticsWindow } from "../bindings/FleetAnalyticsWindow";
import type { FleetJobHistoryPage } from "../bindings/FleetJobHistoryPage";
import type { FleetView } from "../bindings/FleetView";
import activityFixture from "../fixtures/demo-activity.json";
import fixture from "../fixtures/demo-fleet.json";

const ISO = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/;

/** Moves every timestamp in `value` by `deltaMs`, so a fixed fixture looks current. */
function shiftTimestamps<T>(value: T, deltaMs: number): T {
  if (typeof value === "string") {
    return (ISO.test(value) ? new Date(Date.parse(value) + deltaMs).toISOString() : value) as T;
  }
  if (Array.isArray(value)) return value.map((v) => shiftTimestamps(v, deltaMs)) as T;
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, shiftTimestamps(v, deltaMs)])) as T;
  }
  return value;
}

/** Browser-only preview: the demo fixture with timestamps moved to now. */
export function previewFleet(): FleetView {
  const view = fixture as unknown as FleetView;
  return shiftTimestamps(view, Date.now() - Date.parse(view.generatedAt));
}

/** The preview's job history and analytics, generated with the fleet fixture. */
export interface PreviewActivity {
  generatedAt: string;
  history: FleetJobHistoryPage;
  analytics: Record<FleetAnalyticsWindow, FleetAnalytics>;
}

/** Browser-only preview: demo job history and analytics with timestamps moved to now. */
export function previewActivity(): PreviewActivity {
  const activity = activityFixture as unknown as PreviewActivity;
  return shiftTimestamps(activity, Date.now() - Date.parse(activity.generatedAt));
}
