import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import type { FleetView } from "../bindings/FleetView";
import { Button, Card, ErrorText, Pill, Switch, TimeAgo } from "../components/ui";
import { CopyValue } from "../components/CopyValue";
import { commands, isLive } from "../lib/tauri";

/** The opt-in watchdog. Turning it on is the standing approval for its restarts. */
export function WatchdogSettings({ view }: { view: FleetView }) {
  const live = isLive(view);
  const enabled = view.automation.watchdog.enabled;
  const toggle = useMutation({ mutationFn: () => commands.setWatchdog(!enabled) });
  return <>
    <h2 className="mt-5 text-[13px] font-semibold">Watchdog</h2>
    <Card className="mt-2 px-4 py-3 text-[12.5px]">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <p className="font-medium">Automatically restart stopped runner services</p>
          <p className="mt-1 text-[12px] text-ink-2">
            Off by default. When on, the app restarts an inventory runner whose service is seen stopped or failed while GitHub shows it offline and
            not busy, without a review each time: turning this on approves those restarts. It waits 2 minutes, then 5, 15 and 60 minutes
            between attempts, makes at most 3 attempts per runner in 6 hours, then raises an alert and stops trying. It never touches a
            drained, legacy or decommissioned runner, and every attempt is recorded in the machine's history and sent as a notification.
          </p>
        </div>
        <Switch checked={enabled} label="Automatically restart stopped runner services" disabled={!live || toggle.isPending} onChange={() => toggle.mutate()} />
      </div>
      {toggle.isError && <ErrorText error={toggle.error} className="mt-2" />}
      {!live && <p className="mt-2 text-[12px] text-ink-3">Demo data shows a fictional watchdog; it never restarts anything.</p>}
    </Card>
  </>;
}

/** The opt-in organization variable describing self-hosted availability. */
export function PublishSettings({ view }: { view: FleetView }) {
  const live = isLive(view);
  const status = view.automation.publish;
  const [variable, setVariable] = useState(status.variable);
  const [minutes, setMinutes] = useState(String(status.intervalMinutes));
  const save = useMutation({
    mutationFn: (enabled: boolean) => commands.setPublishSettings({ enabled, variable: variable.trim(), intervalMinutes: Number(minutes) }),
  });
  const org = status.organization ?? "your organization";
  const snippet = `runs-on: \${{ vars.${variable || "RUNNER_FLEET_STATUS"} && fromJSON(vars.${variable || "RUNNER_FLEET_STATUS"}).online['macos-build'] > 0 && fromJSON('["self-hosted","macos-build"]') || 'macos-latest' }}`;
  return <>
    <h2 className="mt-5 text-[13px] font-semibold">Fleet availability for workflows</h2>
    <Card className="mt-2 px-4 py-3 text-[12.5px]">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <p className="font-medium">Publish availability to an organization variable</p>
          <p className="mt-1 text-[12px] text-ink-2">
            Off by default. When on, the app writes the organization Actions variable <span className="font-mono">{variable || "RUNNER_FLEET_STATUS"}</span> in {org},
            visible to its private repositories, with the number of inventory runners GitHub shows online per capability label, for example
            <span className="font-mono"> {"{"}"updatedAt":"…","online":{"{"}"macos-build":2,"linux-build":0{"}"},"healthy":false{"}"}</span>. It uses your gh login
            (organization admin), writes only from fresh GitHub data, only when the counts change, and at most every {minutes || "5"} minutes.
            Workflows can then fall back to GitHub-hosted runners.
          </p>
        </div>
        <Switch checked={status.enabled} label="Publish fleet availability" disabled={!live || save.isPending} onChange={() => save.mutate(!status.enabled)} />
      </div>
      <div className="mt-3 flex items-center gap-2">
        <input className="input mono w-[220px]" value={variable} onChange={(e) => setVariable(e.target.value)} aria-label="Variable name" disabled={!live} />
        <span className="text-ink-2">at most every</span>
        <input className="input w-[60px]" value={minutes} onChange={(e) => setMinutes(e.target.value.replace(/\D/g, ""))} aria-label="Minutes between writes" disabled={!live} />
        <span className="text-ink-2">minutes</span>
        <Button small disabled={!live || save.isPending} onClick={() => save.mutate(status.enabled)}>Save</Button>
      </div>
      {save.isError && <ErrorText error={save.error} className="mt-2" />}
      <div className="mt-2 text-[12px] text-ink-2">
        {status.error ? <span className="text-warn selectable">{status.error}</span>
          : status.publishedAt ? <><Pill tone="ok">Published</Pill> <TimeAgo iso={status.publishedAt} /></>
          : status.enabled ? status.waiting ?? "Waiting for the first write." : null}
        {status.value && <div className="mono mt-1 truncate text-[11.5px] text-ink-3 selectable">{status.value}</div>}
      </div>
      <div className="mono mt-2 overflow-hidden rounded-[6px] bg-sunken px-2 py-1.5 text-[11.5px] text-ink">
        <CopyValue value={snippet} label="Workflow snippet" truncate />
      </div>
    </Card>
  </>;
}
