import type { MachineView } from "../../bindings/MachineView";
import { DEFAULT_CLEANUP_OPTIONS, upgradableRunners } from "../../lib/desired";
import { Button, Card, Section } from "../ui";
import type { Change } from "./changes";

/** Reviewed upkeep for an inventory machine: disk cleanup and runner upgrades. */
export function MachineMaintenance({ m, change, enabled }: { m: MachineView; change: Change; enabled: boolean }) {
  const d = m.desired;
  if (!d) return null;
  const behind = upgradableRunners(m);
  const latest = behind[0]?.versionStatus?.latest;
  return (
    <Section
      title="Maintenance"
      action={
        <div className="flex gap-2">
          <Button small disabled={!enabled || change.preparing} onClick={() => change.open({ kind: "cleanup", host: d.host.id, options: DEFAULT_CLEANUP_OPTIONS })}>
            Clean up…
          </Button>
          {behind.length > 0 && (
            <Button small disabled={!enabled || change.preparing} onClick={() => change.open({ kind: "upgrade", host: d.host.id, runner: null })}>
              Upgrade idle runners…
            </Button>
          )}
        </div>
      }
    >
      <Card className="px-4 py-3 text-[12.5px] text-ink-2">
        <p>
          <span className="font-medium text-ink">Clean up</span> measures idle runners' temporary files, downloaded actions and old
          diagnostic logs{d.host.serviceMode === "launchagent" ? ", Xcode DerivedData and unavailable simulators" : ""}
          {d.host.platform.startsWith("windows") ? "" : ", and Docker's unused data (never volumes)"}, and shows each path with its size
          before anything is removed.
        </p>
        <p className="mt-1.5">
          {behind.length > 0
            ? <><span className="font-medium text-ink">{behind.map((r) => r.name).join(", ")}</span> {behind.length === 1 ? "is" : "are"} behind runner {latest}. Upgrade replaces each idle runner's binaries in place, one at a time.</>
            : "Every runner here is on the latest runner release, or its version isn't known yet."}
        </p>
      </Card>
    </Section>
  );
}
