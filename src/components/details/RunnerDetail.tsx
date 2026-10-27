import type { FleetView } from "../../bindings/FleetView";
import type { RunnerView } from "../../bindings/RunnerView";
import type { RunnerDesired } from "../../bindings/RunnerDesired";
import type { Tri } from "../../bindings/Tri";
import { useNow } from "../../hooks";
import { timeAgo } from "../../lib/format";
import { changes, drainOf, driftText, hasLogs, isMaintenance, labelDiff, operationRunning, requestTargetsRunner, roleLabel, runnerActions } from "../../lib/desired";
import { findMachine } from "../../lib/nav";
import { busySince, runnerStatus } from "../../lib/status";
import { commands, isLive, isTauri } from "../../lib/tauri";
import { versionText } from "../../lib/versions";
import { useMutation } from "@tanstack/react-query";
import type { FleetDrain } from "../../bindings/FleetDrain";
import type { FleetWatchdogRunner } from "../../bindings/FleetWatchdogRunner";
import { useUi } from "../../store";
import { Button, Callout, Card, ErrorText, KeyValuePanel, Label, Pill, Section, Stat, StatStrip, type KeyValueRow } from "../ui";
import { Icon } from "../icons";
import { OperationCard, PlanItems, useChange } from "./changes";
import { LogsPanel } from "./LogsPanel";
import { ChangePanel } from "./ReviewCard";
import { JobCard, PageHeader } from "./shared";
import { CopyValue } from "../CopyValue";

export function RunnerDetail({ runner: r, view }: { runner: RunnerView; view: FleetView }) {
  const now = useNow();
  const select = useUi((s) => s.select);
  const st = runnerStatus(r);
  const machine = findMachine(view, r.machineId);
  const since = busySince(r);
  const gh = r.github;
  const local = r.local;
  const change = useChange();
  const op = view.plan.operation;
  const busy = operationRunning(view);
  const live = isLive(view);
  const drain = drainOf(view, r.name);
  // Demo data still shows maintenance actions: their reviews are fictional and can't run.
  const actions = live ? runnerActions(r, drain) : isTauri ? runnerActions(r, drain).filter((a) => isMaintenance(a.request)) : [];
  const watchdog = view.automation.watchdog.runners.find((w) => w.runner === r.name && w.hostId === r.desired?.hostId);
  const labels = r.desired ? labelDiff(r).map((l) => l.label) : gh?.labels ?? [];

  return (
    <div>
      <PageHeader
        titleText={r.name}
        icon={<Icon name="runner" size={16} />}
        title={<CopyValue value={r.name} label="Runner name" className="font-mono text-[18px]" />}
        status={<Pill tone={st.tone} hollow={st.hollow}>{st.label}</Pill>}
        subtitle={<>
          {machine ? <button className="underline-offset-2 hover:text-ink hover:underline" onClick={() => select({ kind: "machine", id: machine.id })}>{machine.displayName}</button> : "Not linked to a machine"}
          {r.org && <> · <CopyValue value={r.org} label="Organization" /></>}{gh && ` · ${gh.os}`}
        </>}
        actions={actions.length > 0 && actions.map((a) => <Button key={a.label} small variant={a.danger ? "danger" : "default"} disabled={busy || change.preparing} onClick={() => change.open(a.request)}>
          {a.request.kind === "restart" && <Icon name="refresh" />}{a.label}…
        </Button>)}
      />
      {op && requestTargetsRunner(op.request, r) && <OperationCard op={op} />}
      <ChangePanel change={change} busy={busy} />

      <div className="mt-8"><StatStrip cols={3}>
        <Fact label="Installed" value={r.installed} detail={local ? local.stale ? `Last read ${timeAgo(local.asOf, now)} (stale)` : local.configured ? "configured on the machine" : "not configured at its inventory path" : "No SSH read yet"} />
        <Fact label="Connected to GitHub" value={r.connected} detail={gh ? `${gh.status}${gh.stale ? `, as of ${timeAgo(gh.asOf, now)} · stale` : ""}` : "Not in GitHub's list"} />
        <Stat
          label="Doing work"
          value={r.activity.kind === "busy" ? "Running job" : r.activity.kind === "unknown" ? "Unknown" : r.activity.kind === "idle" ? "Idle" : "No"}
          tone={r.activity.kind === "unknown" ? "warn" : st.tone}
          hollow={st.hollow}
          sub={r.activity.kind === "busy" || r.activity.kind === "idle"
            ? "Reported by GitHub"
            : r.activity.kind === "offline" ? "Runner is offline" : "No fresh job data"}
        />
      </StatStrip></div>

      {r.versionStatus?.overdue && <Callout tone="warn" title={`Runner update overdue: ${r.versionStatus.latest}`}>
        This runner has {r.versionStatus.current}. GitHub can stop sending jobs to a runner that misses an update for 30 days.
      </Callout>}
      {drain && <DrainNotice drain={drain} live={live} />}
      {watchdog && <WatchdogNotice w={watchdog} enabled={view.automation.watchdog.enabled} />}
      {r.warnings.length > 0 && <div className="mt-4 space-y-2">{r.warnings.map((w) => <Callout key={w} tone="warn" className="">{w}</Callout>)}</div>}

      {r.activity.kind === "busy" && <div className="mt-6">
        {r.currentJob ? <JobCard job={r.currentJob} since={since} /> : <Card className="border-busy/25 bg-busy/5 px-4 py-3 text-[13px] text-ink-2">
          GitHub reports this runner busy. Job details unavailable: the job's repository may be outside the scanned repositories, or the scan hasn't caught up yet.
        </Card>}
      </div>}

      <Section title="State">
        <div className={`grid grid-cols-1 gap-3 ${r.desired ? "min-[1100px]:grid-cols-3" : "min-[1000px]:grid-cols-2"}`}>
          {r.desired && <Desired r={r} d={r.desired} machineName={machine?.displayName ?? null} />}
          <KeyValuePanel title="GitHub" rows={gh ? [
            ["Status", `${gh.status}${gh.busy ? ", busy" : ""}`],
            ["Runner ID", <span className="font-mono text-[12px]">{gh.id}</span>, String(gh.id)],
            ["Registered to", gh.repo ? `Repository ${gh.repo}` : "Organization", gh.repo ?? undefined],
            ["Version", gh.version ? versionText(r, gh.version) : "—"],
            ["Ephemeral", gh.ephemeral == null ? "—" : gh.ephemeral ? "Yes" : "No"],
          ] : []} footer={gh ? <span className={`flex items-center gap-1.5 ${gh.stale ? "text-warn" : ""}`}>{gh.stale && <Icon name="alert" size={12} />}As of {timeAgo(gh.asOf, now)}{gh.stale ? " (stale)" : ""}</span> : r.org ? `Not in ${r.org}'s runner list.` : "Not registered to an organization added to the dashboard."} />
          <KeyValuePanel title="On the machine" rows={local ? [
            ["Configured", local.configured ? "Yes" : "No .runner file"],
            ["Service", local.serviceState],
            ["Version", local.version && !gh?.version ? versionText(r, local.version) : local.version ?? "—"],
            ["Install directory", <span className="font-mono text-[12px]">{local.installDir}</span>, local.installDir],
          ] : []} footer={local ? `As of ${timeAgo(local.asOf, now)}${local.stale ? " (stale)" : ""}`
            : r.desired?.status?.serviceState != null ? `Last check: service ${r.desired.status.serviceState}${r.desired.status.version != null ? `, runner ${r.desired.status.version}` : ""}.`
            : `No SSH read yet. Check ${machine?.displayName ?? "its machine"} to see local details.`} />
        </div>
      </Section>

      {labels.length > 0 && <Section title="Labels" action={<span className="text-[12px] text-ink-2">declared vs GitHub{gh?.stale ? " · last known" : ""}</span>}>
        <Card className="overflow-hidden">
          <ul className="divide-y divide-line">{labels.map((label) => <li key={label} className="grid grid-cols-[minmax(0,1fr)_auto_auto] items-center gap-4 px-4 py-2 text-[13px]">
            <span className="min-w-0"><Label>{label}</Label></span>
            <Presence value={r.desired ? r.desired.desiredLabels.includes(label) : null} label="Declared" />
            <Presence value={gh ? gh.labels.includes(label) : null} label="GitHub" stale={gh?.stale} />
          </li>)}</ul>
        </Card>
      </Section>}

      {live && hasLogs(r) && <LogsPanel runner={r.name} installDir={local?.installDir} />}
    </div>
  );
}

function Desired({ r, d, machineName }: { r: RunnerView; d: RunnerDesired; machineName: string | null }) {
  const items = changes(d.plan);
  const service = d.status?.serviceState;
  const rows: KeyValueRow[] = [
    ["Role", roleLabel(d.role)],
    ["Host", machineName && machineName !== d.hostId ? `${d.hostId} (${machineName})` : d.hostId, d.hostId],
    ["State", d.status ? driftText(r) ?? "In sync" : "Not checked yet"],
  ];
  if (service != null) rows.push(["Service", `${service}${d.status?.version != null ? ` · ${d.status.version}` : ""}`]);
  return <KeyValuePanel title="Inventory" rows={rows} footer={items.length ? <PlanItems items={items} /> : !d.status ? "Check the machine to compare it with the inventory and GitHub." : undefined} />;
}

function Fact({ label, value, detail }: { label: string; value: Tri; detail: string }) {
  return <Stat label={label} value={value === "yes" ? "Yes" : value === "no" ? "No" : "Unknown"} tone={value === "yes" ? "ok" : value === "no" ? "bad" : "warn"} sub={detail} />;
}

function Presence({ value, label, stale }: { value: boolean | null; label: string; stale?: boolean }) {
  return <span className={`flex w-20 items-center gap-1.5 text-[12px] ${value === false ? "text-warn" : "text-ink-2"}`} title={stale ? "From the last GitHub read" : undefined}>
    <span className={value ? "text-ok" : undefined}><Icon name={value == null ? "dash" : value ? "check" : "alert"} size={12} /></span>{label}
    <span className="sr-only">{value == null ? "unknown" : value ? "present" : "missing"}{stale ? " (stale)" : ""}</span>
  </span>;
}

/** A reviewed drain: GitHub can't pause a runner, so its service is stopped on the machine. */
function DrainNotice({ drain, live }: { drain: FleetDrain; live: boolean }) {
  const now = useNow();
  const cancel = useMutation({ mutationFn: () => commands.desiredCancelDrain(drain.runner) });
  return <Callout tone="info" title={drain.state === "draining" ? "Draining after the current job" : "Stopped by a drain"}>
    {drain.state === "draining"
      ? <>Reviewed {timeAgo(drain.requestedAt, now)}. Its service stops on the machine once GitHub shows it idle; GitHub has no way to pause a runner. Until then it keeps working.</>
      : <>Its service was stopped {drain.stoppedAt ? timeAgo(drain.stoppedAt, now) : ""} and stays stopped until you resume it, so GitHub shows it offline. The watchdog leaves it alone. A restart of the machine can start it again.</>}
    {drain.error && <div className="mt-1 text-warn selectable">{drain.error}</div>}
    {drain.state === "draining" && live && <div className="mt-2 flex items-center gap-2">
      <Button small disabled={cancel.isPending} onClick={() => cancel.mutate()}>Cancel drain</Button>
      {cancel.isError && <ErrorText error={cancel.error} />}
    </div>}
  </Callout>;
}

/** What the opt-in watchdog did for this runner in the last six hours. */
function WatchdogNotice({ w, enabled }: { w: FleetWatchdogRunner; enabled: boolean }) {
  const now = useNow();
  if (w.gaveUpAt) {
    return <Callout tone="bad" title="Automatic restarts stopped">
      The watchdog restarted it {w.attempts.length} times in six hours and its service is still stopped. Check its diagnostic log, then restart it here.
      {w.lastError && <div className="mt-1 selectable">{w.lastError}</div>}
    </Callout>;
  }
  if (!w.attempts.length && !w.downSince) return null;
  return <Callout tone={w.downSince ? "warn" : "info"} title={w.downSince ? "Service stopped; the watchdog will restart it" : "Restarted automatically"}>
    {w.attempts.length} automatic restart{w.attempts.length === 1 ? "" : "s"} in the last six hours{w.attempts.length ? `, the last ${timeAgo(w.attempts[w.attempts.length - 1], now)}` : ""}.
    {w.nextAttemptAt && enabled && <> Next attempt {timeAgo(w.nextAttemptAt, now)} if it's still stopped.</>}
    {w.lastError && <div className="mt-1 text-warn selectable">{w.lastError}</div>}
  </Callout>;
}
