import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import type { FleetAlertDestinationKind } from "../bindings/FleetAlertDestinationKind";
import type { FleetAlertDestinationView } from "../bindings/FleetAlertDestinationView";
import type { FleetView } from "../bindings/FleetView";
import { Button, Card, ErrorText, Pill, Switch, TimeAgo } from "../components/ui";
import { commands, isLive } from "../lib/tauri";

const KINDS: Record<FleetAlertDestinationKind, { label: string; url: string }> = {
  slack: { label: "Slack", url: "Incoming webhook URL (https://hooks.slack.com/services/…)" },
  discord: { label: "Discord", url: "Channel webhook URL (https://discord.com/api/webhooks/…)" },
  ntfy: { label: "ntfy", url: "" },
  webhook: { label: "JSON webhook", url: "URL that accepts a JSON POST" },
};

/** Slack, Discord, ntfy and JSON webhooks, besides desktop notifications. */
export function AlertDestinations({ view }: { view: FleetView }) {
  const live = isLive(view);
  const [editing, setEditing] = useState<FleetAlertDestinationView | "new" | null>(null);
  const destinations = view.automation.destinations;
  return <>
    <h2 className="mt-5 text-[13px] font-semibold">Alert destinations</h2>
    <p className="mt-1 text-[12px] text-ink-2">
      The same alerts and recoveries as desktop notifications (offline, low disk, and automatic-restart problems) can also go to Slack, Discord, ntfy or
      any JSON webhook. Each alert is saved before it is sent, retried for a day if a destination fails, and paused for a machine in maintenance.
      Webhook URLs and ntfy tokens are kept in the macOS Keychain, never in the settings file.
    </p>
    <Card className="mt-3 divide-y divide-line">
      {destinations.length === 0 && <div className="px-4 py-3 text-[12.5px] text-ink-2">No destinations yet.</div>}
      {destinations.map((d) => <DestinationRow key={d.id} d={d} live={live} onEdit={() => setEditing(d)} />)}
      <div className="px-4 py-3">
        {editing ? <DestinationForm key={editing === "new" ? "new" : editing.id} existing={editing === "new" ? null : editing} close={() => setEditing(null)} />
          : <Button small disabled={!live} onClick={() => setEditing("new")}>Add destination…</Button>}
        {!live && <p className="mt-2 text-[12px] text-ink-3">Turn off demo data to change destinations. Demo destinations are fictional and never receive anything.</p>}
      </div>
    </Card>
  </>;
}

function DestinationRow({ d, live, onEdit }: { d: FleetAlertDestinationView; live: boolean; onEdit: () => void }) {
  const toggle = useMutation({ mutationFn: () => commands.saveAlertDestination({ id: d.id, name: d.name, kind: null, enabled: !d.enabled, url: null, ntfyServer: null, ntfyTopic: null, token: null }) });
  const test = useMutation({ mutationFn: () => commands.testAlertDestination(d.id) });
  const remove = useMutation({ mutationFn: () => commands.removeAlertDestination(d.id) });
  return <div className="px-4 py-3 text-[12.5px]">
    <div className="flex items-center gap-2">
      <Pill tone="muted">{KINDS[d.kind].label}</Pill>
      <span className="font-medium">{d.name}</span>
      <span className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-ink-3">{d.summary}</span>
      <Button small variant="ghost" disabled={!live || test.isPending} onClick={() => test.mutate()}>{test.isPending ? "Sending…" : "Send test"}</Button>
      <Button small variant="ghost" disabled={!live} onClick={onEdit}>Edit…</Button>
      <Button small variant="ghost" disabled={!live || remove.isPending} onClick={() => remove.mutate()}>Remove</Button>
      <Switch checked={d.enabled} label={`Send alerts to ${d.name}`} disabled={!live || toggle.isPending} onChange={() => toggle.mutate()} />
    </div>
    <div className="mt-1 text-[11.5px] text-ink-3">
      {d.error ? <span className="text-warn selectable">{d.error}</span>
        : d.deliveredAt ? <>Last delivered <TimeAgo iso={d.deliveredAt} /></> : "Nothing delivered yet."}
      {d.pending > 0 && <> · {d.pending} waiting to be sent</>}
    </div>
    {test.isSuccess && <p role="status" className="mt-1 text-[12px] text-ink-2">Test sent.</p>}
    {[test, toggle, remove].map((m, n) => m.isError && <ErrorText key={n} error={m.error} className="mt-1" />)}
  </div>;
}

function DestinationForm({ existing, close }: { existing: FleetAlertDestinationView | null; close: () => void }) {
  const [kind, setKind] = useState<FleetAlertDestinationKind>(existing?.kind ?? "slack");
  const [name, setName] = useState(existing?.name ?? "");
  const [url, setUrl] = useState("");
  const [server, setServer] = useState("");
  const [topic, setTopic] = useState("");
  const [token, setToken] = useState("");
  const save = useMutation({
    mutationFn: () => commands.saveAlertDestination({
      id: existing?.id ?? null,
      name,
      kind,
      enabled: existing?.enabled ?? true,
      url: kind === "ntfy" || !url ? null : url,
      ntfyServer: kind === "ntfy" && server ? server : null,
      ntfyTopic: kind === "ntfy" && topic ? topic : null,
      token: kind === "ntfy" && token ? token : null,
    }),
    onSuccess: close,
  });
  return <div className="space-y-2 text-[12.5px]">
    <div className="flex items-center gap-2">
      <select className="input w-[150px]" value={kind} disabled={!!existing} onChange={(e) => setKind(e.target.value as FleetAlertDestinationKind)}>
        {Object.entries(KINDS).map(([k, v]) => <option key={k} value={k}>{v.label}</option>)}
      </select>
      <input className="input w-[220px]" placeholder="Name, such as #ci-alerts" value={name} onChange={(e) => setName(e.target.value)} />
    </div>
    {kind === "ntfy" ? <>
      <div className="flex items-center gap-2">
        <input className="input w-[220px]" placeholder={existing ? "Server (keep the saved one)" : "Server (https://ntfy.sh)"} value={server} onChange={(e) => setServer(e.target.value)} />
        <input className="input w-[180px]" placeholder={existing ? existing.summary.split("/").pop() : "Topic"} value={topic} onChange={(e) => setTopic(e.target.value)} />
      </div>
      <input type="password" autoComplete="off" className="input w-[410px]" placeholder={existing ? "Access token (leave empty to keep the saved one)" : "Access token (optional)"} value={token} onChange={(e) => setToken(e.target.value)} />
    </> : <input type="password" autoComplete="off" className="input w-[410px]" placeholder={existing ? "Leave empty to keep the saved URL" : KINDS[kind].url} value={url} onChange={(e) => setUrl(e.target.value)} />}
    <p className="text-[11.5px] text-ink-3">Saved in the Keychain. Only HTTPS endpoints are accepted.</p>
    <div className="flex items-center gap-2">
      <Button small variant="primary" disabled={save.isPending || !name.trim()} onClick={() => save.mutate()}>{save.isPending ? "Saving…" : "Save"}</Button>
      <Button small variant="ghost" onClick={close}>Cancel</Button>
      {save.isError && <ErrorText error={save.error} />}
    </div>
  </div>;
}
