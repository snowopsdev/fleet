import { describe, expect, it } from "vitest";
import { findRunner } from "./nav";
import { previewFleet } from "./preview";
import { releaseSummary, runnersBehind, versionText } from "./versions";

describe("runner versions", () => {
  it("lists runners behind the latest release, overdue first", () => {
    const view = previewFleet();
    // The demo's legacy runner, still on its old machine install, is eight releases behind.
    expect(runnersBehind(view).map((r) => [r.name, r.versionStatus?.overdue])).toEqual([["Studio-Old-2", true], ["linux-01-a", false]]);
    expect(releaseSummary(view)).toBe("2 runners are behind runner 2.329.0 (1 overdue).");
  });

  it("describes current, outdated and overdue versions", () => {
    const view = previewFleet();
    const current = findRunner(view, "gh:acme-demo:101")!;
    expect(versionText(current, current.github?.version)).toBe("2.329.0 (latest)");
    const behind = findRunner(view, "gh:acme-demo:301")!;
    expect(versionText(behind, behind.github?.version)).toBe("2.328.0 · 2.329.0 available");
    behind.versionStatus = { ...behind.versionStatus!, overdue: true };
    expect(versionText(behind, behind.github?.version)).toBe("2.328.0 · 2.329.0 available, update overdue");
    expect(releaseSummary(view)).toBe("2 runners are behind runner 2.329.0 (2 overdue).");
  });

  it("says nothing before the latest release is known", () => {
    const view = previewFleet();
    view.runnerRelease = null;
    expect(releaseSummary(view)).toBeNull();
  });
});
