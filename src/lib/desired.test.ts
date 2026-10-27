import { describe, expect, it } from "vitest";
import { drainOf, driftText, formatKib, groupByStem, isMaintenance, requestTitle, upgradableRunners, labelDiff, machineNeedsChanges, requestTargetsMachine, requestTargetsRunner, roleLabel, runnerActions } from "./desired";
import { previewFleet } from "./preview";

const view = previewFleet();
const machine = (name: string) => view.machines.find((m) => m.displayName === name)!;
const runner = (name: string) => view.machines.flatMap((m) => m.runners).find((r) => r.name === name)!;

describe("runnerActions", () => {
  it("offers retire on the legacy runner, naming its replacement", () => {
    expect(runnerActions(runner("Studio-Old-2"))).toEqual([
      { label: "Retire", request: { kind: "retire", host: "studio", runner: "studio-2" }, danger: true },
      { label: "Restart", request: { kind: "restart", runner: "Studio-Old-2" } },
    ]);
  });

  it("offers the apply step for drift and never restarts a busy runner", () => {
    expect(runnerActions(runner("mini-02"))).toEqual([
      { label: "Fix labels", request: { kind: "apply", host: "mac-mini-02", runner: "mini-02" } },
      { label: "Drain", request: { kind: "drain", runner: "mini-02" } },
    ]);
    // A busy runner can only be drained: its service stops after the job.
    expect(runnerActions(runner("studio-1"))).toEqual([{ label: "Drain", request: { kind: "drain", runner: "studio-1" } }]);
  });

  it("offers decommission only for declared runners, and nothing for unreachable hosts", () => {
    const checked = structuredClone(runner("linux-01-b"));
    checked.desired!.plan = [{ ...checked.desired!.plan[0], action: "decommission", apply: false, blocked: false }];
    expect(runnerActions(checked).map((a) => a.label)).toEqual(["Decommission"]);
    checked.desired!.role = { kind: "undeclared" };
    expect(runnerActions(checked)).toEqual([]);
    expect(runnerActions(runner("linux-01-b"))).toEqual([]);
    expect(runnerActions(runner("win-builder"))).toEqual([]);
  });

  it("requires an unblocked apply classification supplied by the plan", () => {
    const checked = structuredClone(runner("mini-03-01"));
    const item = { ...checked.desired!.plan[0], action: "start-service" as const, apply: true, blocked: true };
    checked.desired!.plan = [item];
    expect(runnerActions(checked)).toEqual([]);
    item.blocked = false;
    expect(runnerActions(checked)).toEqual([
      { label: "Start service", request: { kind: "apply", host: "mac-mini-03", runner: "mini-03-01" } },
    ]);
    item.apply = false;
    expect(runnerActions(checked)).toEqual([]);
  });

  it("offers install for a declared runner that isn't there yet", () => {
    const checked = structuredClone(runner("mini-03-01"));
    checked.desired!.plan = [{ ...checked.desired!.plan[0], action: "install", apply: true, blocked: false }];
    expect(runnerActions(checked)).toEqual([
      { label: "Install", request: { kind: "apply", host: "mac-mini-03", runner: "mini-03-01" } },
    ]);
    // An SSH failure cannot establish that the runner is missing.
    expect(runnerActions(runner("mini-03-01"))).toEqual([]);
  });
});

describe("desired state on runners and machines", () => {
  it("describes roles and drift", () => {
    expect(roleLabel(runner("Studio-Old-2").desired!.role)).toBe("Legacy runner, replaced by studio-2");
    expect(driftText(runner("studio-2"))).toBeNull();
    expect(driftText(runner("mini-03-01"))).toBe("host unavailable");
    expect(driftText(runner("win-builder"))).toBe("host unavailable");
  });

  it("flags machines with changes or that couldn't be read", () => {
    expect(machineNeedsChanges(machine("mac-mini-02"))).toBe(true);
    expect(machineNeedsChanges(machine("win-builder"))).toBe(true); // couldn't be read
  });

  it("compares declared labels with GitHub's", () => {
    const states = Object.fromEntries(labelDiff(runner("mini-02")).map((l) => [l.label, l.state]));
    expect(states["macos-build"]).toBe("missing");
    expect(states["self-hosted"]).toBeUndefined(); // GitHub's own label, not managed
  });
});

describe("which pages show a change", () => {
  it("shows a restart on its runner and on the machine it runs on", () => {
    const restart = { kind: "restart", runner: "studio-1" } as const;
    expect(requestTargetsRunner(restart, runner("studio-1"))).toBe(true);
    expect(requestTargetsRunner(restart, runner("studio-2"))).toBe(false);
    expect(requestTargetsMachine(restart, machine("studio"))).toBe(true);
    expect(requestTargetsMachine(restart, machine("mac-mini-02"))).toBe(false);
  });

  it("matches host changes by inventory host, including a host-wide apply on each runner", () => {
    const apply = { kind: "apply", host: "studio", runner: null } as const;
    expect(requestTargetsMachine(apply, machine("studio"))).toBe(true);
    expect(requestTargetsMachine(apply, view.otherMachines[0])).toBe(false);
    expect(requestTargetsRunner(apply, runner("studio-2"))).toBe(true);
    expect(requestTargetsRunner({ ...apply, runner: "studio-1" }, runner("studio-2"))).toBe(false);
    expect(requestTargetsRunner({ kind: "decommission", host: "build-linux-01", runner: "linux-01-b" }, runner("linux-01-b"))).toBe(true);
  });

  it("shows a retire on the legacy runner it removes", () => {
    const retire = { kind: "retire", host: "studio", runner: "studio-2" } as const;
    expect(requestTargetsRunner(retire, runner("Studio-Old-2"))).toBe(true);
    expect(requestTargetsRunner(retire, runner("studio-1"))).toBe(false);
  });
});

describe("groupByStem", () => {
  it("groups by name stem with counts", () => {
    const runners = ["depot-a1", "depot-b2", "depot-c3"].map((name, i) => ({
      ...view.unmatchedRunners[0],
      key: `k${i}`,
      name,
    }));
    expect(groupByStem([...runners, view.unmatchedRunners[0]])).toMatchObject([
      { name: "depot-*", runners: { length: 3 } },
      { name: "cloud-gpu-1", runners: { length: 1 } },
    ]);
  });
});

describe("maintenance actions", () => {
  it("offers upgrade only to idle runners behind the latest release", () => {
    // Its machine is unreachable in the demo, so nothing is offered until a check reads it.
    expect(runnerActions(runner("linux-01-a"))).toEqual([]);
    const checked = structuredClone(runner("linux-01-a"));
    checked.desired!.status!.serviceState = "active";
    expect(runnerActions(checked).map((a) => a.label)).toEqual(["Restart", "Upgrade", "Drain"]);
    expect(runnerActions(runner("studio-2")).map((a) => a.label)).not.toContain("Upgrade");
    expect(upgradableRunners(machine("build-linux-01")).map((r) => r.name)).toEqual(["linux-01-a"]);
    checked.activity = { kind: "busy", since: null };
    expect(runnerActions(checked).map((a) => a.label)).toEqual(["Drain"]);
  });

  it("shows a drained runner as such and offers resume, not another drain", () => {
    const drain = drainOf(view, "studio-1");
    expect(drain?.state).toBe("draining");
    expect(runnerActions(runner("studio-1"), drain)).toEqual([]);
    const stopped = { ...drain!, state: "stopped" as const };
    expect(runnerActions(runner("studio-1"), stopped)).toEqual([{ label: "Resume", request: { kind: "resume", runner: "studio-1" } }]);
    expect(drainOf(view, "studio-2")).toBeNull();
  });

  it("titles and targets maintenance requests", () => {
    const cleanup = { kind: "cleanup", host: "studio", options: { wholeWork: false, diagOlderThanDays: 14 } } as const;
    expect(requestTitle(cleanup)).toBe("Clean up studio");
    expect(requestTitle({ kind: "upgrade", host: "studio", runner: null })).toBe("Upgrade idle runners");
    expect(requestTargetsMachine({ kind: "drain", runner: "studio-1" }, machine("studio"))).toBe(true);
    expect(requestTargetsRunner({ kind: "upgrade", host: "build-linux-01", runner: null }, runner("linux-01-a"))).toBe(true);
    expect(requestTargetsRunner({ kind: "resume", runner: "studio-2" }, runner("studio-1"))).toBe(false);
    expect(isMaintenance(cleanup)).toBe(true);
    expect(isMaintenance({ kind: "restart", runner: "studio-1" })).toBe(false);
  });

  it("formats sizes like the host scripts", () => {
    expect(formatKib(512)).toBe("512 KiB");
    expect(formatKib(2048)).toBe("2.0 MiB");
    expect(formatKib(2 * 1024 * 1024)).toBe("2.0 GiB");
  });
});
