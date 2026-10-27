import { describe, expect, it } from "vitest";
import { previewFleet } from "./preview";
import { diskUsedFraction, memoryUsedFraction, primaryDisk, runnerDisk } from "./health";

const health = previewFleet().machines.find((machine) => machine.health)!.health!;
describe("shared health selectors", () => {
  it.each(["/", "C:", "C:\\", "c:/"])("selects root %s over a subdirectory", (root) => {
    const disks = [
      { mountPoint: "C:\\workspace", availableBytes: 1, totalBytes: 10, runnerRoot: false },
      { mountPoint: root, availableBytes: 2, totalBytes: 10, runnerRoot: false },
    ];
    expect(primaryDisk({ ...health, disks })).toBe(disks[1]);
  });
  it("falls back to the first disk and preserves missing metrics", () => {
    expect(primaryDisk(health)).toBe(health.disks[0]);
    expect(primaryDisk(null)).toBeUndefined();
    expect(memoryUsedFraction(null)).toBeNull();
    expect(memoryUsedFraction({ ...health, memoryAvailableBytes: null })).toBeNull();
    expect(diskUsedFraction(undefined)).toBeNull();
  });
  it("computes memory and disk usage consistently", () => {
    expect(memoryUsedFraction({ ...health, memoryTotalBytes: 100, memoryAvailableBytes: 25 })).toBe(0.75);
    expect(diskUsedFraction({ mountPoint: "/", totalBytes: 100, availableBytes: 25, runnerRoot: false })).toBe(0.75);
  });
  it("shows a separate runner volume only when the runner root isn't on the primary disk", () => {
    const system = { mountPoint: "/", availableBytes: 5, totalBytes: 10, runnerRoot: false };
    const external = { mountPoint: "/Volumes/Builds", availableBytes: 1, totalBytes: 10, runnerRoot: true };
    expect(runnerDisk({ ...health, disks: [system, external] })).toBe(external);
    expect(runnerDisk({ ...health, disks: [{ ...system, runnerRoot: true }] })).toBeUndefined();
  });
});
