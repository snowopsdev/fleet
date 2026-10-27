import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { AppConfig } from "../bindings/AppConfig";
import type { FleetHostedRates } from "../bindings/FleetHostedRates";
import type { FleetJobFeedStatus } from "../bindings/FleetJobFeedStatus";
import type { FleetView } from "../bindings/FleetView";
import { Button, Card, ErrorText, Pill, Switch } from "../components/ui";
import { CopyValue } from "../components/CopyValue";
import { parseRate } from "../lib/activity";
import { commands, isTauri } from "../lib/tauri";

/** GitHub's published standard runner rates; matches `FleetHostedRates::default` in Rust. */
export const DEFAULT_RATES: FleetHostedRates = { linux: 0.006, windows: 0.01, macos: 0.062, asOf: "2026-10-26" };

const EXTENSION_INSTALL = "gh extension install cli/gh-webhook";
const HOOK_SCOPE = "admin:org_hook";

export function ActivitySettings({ config, view }: { config: AppConfig; view: FleetView }) {
  return (
    <>
      <h2 className="text-[13px] font-semibold">GitHub-hosted cost estimate</h2>
      <p className="mt-1 text-[12px] text-ink-2">
        Per-minute USD rates used on the Activity page to estimate what your self-hosted jobs would cost on GitHub-hosted
        standard runners. Defaults are GitHub's published rates for 2-core Linux and Windows and 3- or 4-core macOS runners.
      </p>
      <Rates key={JSON.stringify(config.jobActivity.hostedRates)} rates={config.jobActivity.hostedRates} />

      <h2 className="mt-5 text-[13px] font-semibold">Real-time job feed</h2>
      <p className="mt-1 text-[12px] text-ink-2">
        Off by default. Job history comes from scanning GitHub every 45 seconds, which can miss very short jobs. The feed adds
        GitHub's <code>workflow_job</code> events as they happen.
      </p>
      <ul className="mt-2 list-disc space-y-1 pl-5 text-[12px] text-ink-2">
        <li>While the app is open, it runs <code>gh webhook forward --org=ORG --events=workflow_job</code> for each organization you turn on. That creates a temporary webhook on the organization, which GitHub delivers to its forwarding service and relays to this Mac only. The app stops it when it quits.</li>
        <li>It needs the GitHub CLI extension (<CopyValue value={EXTENSION_INSTALL} label="Command"><code>{EXTENSION_INSTALL}</code></CopyValue>) and the <code>{HOOK_SCOPE}</code> scope on your gh login, as an organization owner.</li>
        <li>GitHub allows one forwarder per organization at a time and describes it as a development tool. It disconnects every few minutes; the app restarts it, and keeps scanning every 3 minutes as a fallback.</li>
      </ul>
      <HookScope />
      <div className="mt-3 space-y-2">
        {config.orgs.length === 0 && <Card className="px-4 py-3 text-[12.5px] text-ink-2">Add an organization in the GitHub tab first.</Card>}
        {config.orgs.filter((o) => o.kind !== "user").map((o) => {
          const key = o.login.toLowerCase();
          return <FeedRow key={key} org={o.login} enabled={config.jobActivity.feedOrgs.includes(key)} demo={config.demoMode} status={view.activity.feeds.find((f) => f.org === key)} />;
        })}
      </div>
    </>
  );
}

function Rates({ rates }: { rates: FleetHostedRates }) {
  const [form, setForm] = useState({ linux: String(rates.linux), windows: String(rates.windows), macos: String(rates.macos), asOf: rates.asOf });
  const save = useMutation({ mutationFn: (next: FleetHostedRates) => commands.setHostedRates(next) });
  const parsed = { linux: parseRate(form.linux), windows: parseRate(form.windows), macos: parseRate(form.macos) };
  const valid = parsed.linux != null && parsed.windows != null && parsed.macos != null && form.asOf.trim() !== "";
  const field = (key: "linux" | "windows" | "macos", label: string) => (
    <label className="flex flex-col gap-1 text-[12px]">
      <span className="font-medium">{label}</span>
      <span className="flex items-center gap-1"><span className="text-ink-2">$</span>
        <input className="input w-[90px]" inputMode="decimal" aria-invalid={parsed[key] == null} value={form[key]} onChange={(e) => setForm({ ...form, [key]: e.target.value })} />
        <span className="text-ink-3">/min</span></span>
    </label>
  );
  return (
    <Card className="mt-2 px-4 py-3">
      <form className="flex flex-wrap items-end gap-4" onSubmit={(e) => {
        e.preventDefault();
        if (valid) save.mutate({ linux: parsed.linux!, windows: parsed.windows!, macos: parsed.macos!, asOf: form.asOf.trim() });
      }}>
        {field("linux", "Linux")}
        {field("windows", "Windows")}
        {field("macos", "macOS")}
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="font-medium">Rates as of</span>
          <input className="input w-[120px]" value={form.asOf} onChange={(e) => setForm({ ...form, asOf: e.target.value })} />
        </label>
        <div className="flex gap-2">
          <Button type="submit" variant="primary" disabled={!isTauri || !valid || save.isPending}>{save.isPending ? "Saving…" : "Save"}</Button>
          <Button variant="ghost" disabled={!isTauri || save.isPending} onClick={() => {
            setForm({ linux: String(DEFAULT_RATES.linux), windows: String(DEFAULT_RATES.windows), macos: String(DEFAULT_RATES.macos), asOf: DEFAULT_RATES.asOf });
            save.mutate(DEFAULT_RATES);
          }}>Use GitHub's rates</Button>
        </div>
      </form>
      {!valid && <p className="mt-2 text-[11.5px] text-warn">Enter each rate as dollars per minute between 0 and 10, and when you checked them.</p>}
      {save.isError && <ErrorText error={save.error} className="mt-2" />}
      <p className="mt-2 text-[11.5px] text-ink-3">An estimate only: hosted runners differ in size and speed, and plan-included minutes aren't subtracted.</p>
    </Card>
  );
}

function HookScope() {
  const { data: gh } = useQuery({ queryKey: ["gh-status"], queryFn: commands.getGhStatus, enabled: isTauri });
  if (!gh?.login || gh.scopes.includes(HOOK_SCOPE)) return null;
  const fix = `gh auth refresh --hostname ${gh.host} --scopes ${HOOK_SCOPE}`;
  return (
    <div className="mt-2 break-words rounded-[8px] border border-warn/30 bg-warn/5 px-3 py-2 text-[12px]">
      Your gh login ({gh.login}) doesn't have <code>{HOOK_SCOPE}</code> yet. Run in Terminal: <CopyValue value={fix} label="Command"><code className="rounded bg-card px-1">{fix}</code></CopyValue>
    </div>
  );
}

function FeedRow({ org, enabled, demo, status }: { org: string; enabled: boolean; demo: boolean; status?: FleetJobFeedStatus }) {
  const toggle = useMutation({ mutationFn: () => commands.setJobFeed(org, !enabled) });
  return (
    <Card className="px-4 py-3 text-[12.5px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">{org}</span>
        {enabled && (demo ? <Pill tone="muted">Paused for demo data</Pill>
          : !status ? <Pill tone="muted">Starting</Pill>
          : status.state === "forwarding" ? <Pill tone="ok">Connected</Pill>
          : status.state === "unavailable" ? <Pill tone="bad">Unavailable</Pill>
          : <Pill tone="warn">{status.state === "starting" ? "Starting" : "Reconnecting"}</Pill>)}
        <span className="ml-auto" />
        <Switch checked={enabled} label={`Real-time job feed for ${org}`} disabled={!isTauri || toggle.isPending} onChange={() => toggle.mutate()} />
      </div>
      {enabled && status?.detail && <div className="mt-1 break-words text-[11.5px] text-ink-2 selectable">{status.detail}</div>}
      {enabled && status?.fix && <div className="mt-1 break-words text-[11.5px]">Run in Terminal: <CopyValue value={status.fix} label="Command"><code className="rounded bg-sunken px-1">{status.fix}</code></CopyValue></div>}
      {enabled && status && status.restarts > 0 && <div className="mt-1 text-[11px] text-ink-3">Restarted {status.restarts} {status.restarts === 1 ? "time" : "times"}{status.lastDeliveryAt ? ` · last event ${new Date(status.lastDeliveryAt).toLocaleTimeString()}` : ""}</div>}
      {toggle.isError && <ErrorText error={toggle.error} className="mt-2" />}
    </Card>
  );
}
