import { keepPreviousData, useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { FleetAnalytics } from "../../bindings/FleetAnalytics";
import type { FleetAnalyticsWindow } from "../../bindings/FleetAnalyticsWindow";
import type { FleetCompletedJob } from "../../bindings/FleetCompletedJob";
import type { FleetRunnerTimeShare } from "../../bindings/FleetRunnerTimeShare";
import type { FleetView } from "../../bindings/FleetView";
import { barFractions, formatDuration, formatHours, formatShare, formatUsd, osLabel, WINDOWS } from "../../lib/activity";
import { formatClock, plural } from "../../lib/format";
import { commands, emitEvent, isTauri } from "../../lib/tauri";
import { useUi } from "../../store";
import { Button, Callout, Card, EmptyLine, ErrorText, Label, Meter, Pill, Section, Stat, StatStrip } from "../ui";
import { JobRows } from "./RecentJobs";
import { PageHeader } from "./shared";

export function ActivityDetail({ view }: { view: FleetView }) {
  const [window, setWindow] = useState<FleetAnalyticsWindow>("24h");
  const latest = view.activity.recent[0]?.id ?? null;
  const analytics = useQuery({
    queryKey: ["analytics", window, view.source, latest],
    queryFn: () => commands.getAnalytics(window),
    placeholderData: keepPreviousData,
    refetchInterval: 60_000,
  });
  const since = view.activity.historySince;
  return (
    <div>
      <PageHeader
        title="Activity"
        subtitle={`Self-hosted jobs recorded on this Mac${since ? ` since ${new Date(since).toLocaleDateString()}` : ""}. Up to 30 days, 20,000 jobs.`}
        actions={<WindowPicker value={window} onChange={setWindow} />}
      />
      {view.activity.storageError && <Callout tone="warn" title="Job history isn't being saved">{view.activity.storageError}</Callout>}
      <FeedLine view={view} />
      {analytics.isError && <ErrorText error={analytics.error} className="mt-4" />}
      {analytics.data ? <ActivityContent a={analytics.data} view={view} /> : !analytics.isError && <Card className="mt-8 px-4 py-6 text-[13px] text-ink-2">Summarizing job history…</Card>}
      <JobHistory key={`${view.source}:${latest}`} view={view} />
    </div>
  );
}

function WindowPicker({ value, onChange }: { value: FleetAnalyticsWindow; onChange: (w: FleetAnalyticsWindow) => void }) {
  return (
    <div role="radiogroup" aria-label="Time window" className="flex rounded-[7px] border border-line bg-sunken p-0.5">
      {WINDOWS.map((w) => (
        <button key={w.value} role="radio" aria-checked={value === w.value} onClick={() => onChange(w.value)}
          className={`rounded-[5px] px-2.5 py-1 text-[12px] font-medium ${value === w.value ? "bg-card text-ink shadow-xs" : "text-ink-2 hover:text-ink"}`}>
          {w.label}
        </button>
      ))}
    </div>
  );
}

function FeedLine({ view }: { view: FleetView }) {
  const feeds = view.activity.feeds;
  if (view.source === "demo") return null;
  const text = feeds.length === 0
    ? "Jobs are found by scanning GitHub every 45 seconds; very short jobs between scans can be missed."
    : feeds.map((f) => `${f.org}: ${f.state === "forwarding" ? "real-time feed connected" : f.state === "unavailable" ? `real-time feed unavailable${f.detail ? ` (${f.detail})` : ""}` : "real-time feed reconnecting"}`).join(" · ");
  return (
    <p className="mt-2 flex flex-wrap items-center gap-2 text-[12px] text-ink-2">
      {text}
      {isTauri && <button className="text-accent hover:underline" onClick={() => emitEvent("open-settings")}>{feeds.length ? "Feed settings" : "Real-time feed…"}</button>}
    </p>
  );
}

/** The window's summary: totals, utilization over time, machines, runners, waits, top work and cost. */
export function ActivityContent({ a, view }: { a: FleetAnalytics; view: FleetView }) {
  const select = useUi((s) => s.select);
  const t = a.totals;
  const busySecs = a.runners.reduce((sum, r) => sum + r.busySecs, 0);
  const capacity = a.observedFrom ? (Date.parse(a.to) - Date.parse(a.observedFrom)) / 1000 * a.runners.length : 0;
  const window = WINDOWS.find((w) => w.value === a.window)!;
  const listed = new Set([...view.machines, ...view.otherMachines].flatMap((m) => m.runners.map((r) => r.key)).concat(view.unmatchedRunners.map((r) => r.key)));
  return (
    <>
      <div className="mt-8"><StatStrip>
        <Stat label="Jobs" value={t.jobs.toLocaleString("en-US")} sub={t.successRate == null ? "none finished" : `${formatShare(t.successRate * 100)} succeeded · ${t.failed} failed`} />
        <Stat label="Runner time" value={formatHours(t.runSecs)} sub={capacity > 0 ? `${formatShare(busySecs / capacity * 100)} busy across ${plural(a.runners.length, "runner")}` : "no recorded time yet"} />
        <Stat label="Queue wait" value={formatDuration(t.queueP50Secs)} sub={t.queueP95Secs == null ? "median" : `median · p95 ${formatDuration(t.queueP95Secs)}`} />
        <Stat label="GitHub-hosted equivalent" value={formatUsd(a.hostedCost.usd)} sub={`estimate · rates as of ${a.hostedCost.rates.asOf}`} />
      </StatStrip></div>
      {a.observedFrom && a.observedFrom !== a.from && <p className="mt-2 text-[12px] text-ink-2">History on this Mac covers only part of {window.long}; percentages use the recorded time since {new Date(a.observedFrom).toLocaleString()}.</p>}

      <Section title="Fleet busy time" action={<span className="text-[12px] text-ink-2">share of runner capacity per {a.window === "24h" ? "hour" : a.window === "7d" ? "6 hours" : "day"}</span>}>
        <BusyChart a={a} />
      </Section>

      <Section title="Machines" count={a.machines.length}>
        <Card className="overflow-x-auto">
          <table className="w-full min-w-[560px] text-[13px]">
            <thead className="border-b border-line bg-sunken/40 text-left text-[12px] text-ink-2"><tr>
              <th scope="col" className="px-4 py-2 font-medium">Machine</th><th scope="col" className="px-3 py-2 font-medium">Busy</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Runner time</th><th scope="col" className="px-3 py-2 text-right font-medium">Jobs</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Succeeded</th>
            </tr></thead>
            <tbody className="divide-y divide-line">
              {a.machines.length === 0 && <tr><td colSpan={5}><EmptyLine>No machine has recorded jobs in {window.long}.</EmptyLine></td></tr>}
              {a.machines.map((m) => {
                const machine = [...view.machines, ...view.otherMachines].find((x) => x.monitoring.machineKey === m.machineKey);
                return <tr key={m.machineKey}>
                  <td className="px-4 py-2.5">{machine ? <button className="font-medium hover:underline" onClick={() => select({ kind: "machine", id: machine.id })}>{m.name}</button> : <span className="font-medium">{m.name}</span>}
                    <div className="text-[11.5px] text-ink-3">{plural(m.runners, "runner")}</div></td>
                  <td className="w-[30%] px-3 py-2.5"><BusyMeter percent={m.busyPercent} /></td>
                  <td className="px-3 py-2.5 text-right tabular">{formatHours(m.busySecs)}</td>
                  <td className="px-3 py-2.5 text-right tabular">{m.totals.jobs}</td>
                  <td className="px-3 py-2.5 text-right tabular">{m.totals.successRate == null ? "—" : formatShare(m.totals.successRate * 100)}</td>
                </tr>;
              })}
            </tbody>
          </table>
        </Card>
      </Section>

      <Section title="Runners" count={a.runners.length}>
        <Card className="overflow-x-auto">
          <table className="w-full min-w-[640px] text-[13px]">
            <thead className="border-b border-line bg-sunken/40 text-left text-[12px] text-ink-2"><tr>
              <th scope="col" className="px-4 py-2 font-medium">Runner</th><th scope="col" className="px-3 py-2 font-medium">Busy</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Jobs</th><th scope="col" className="px-3 py-2 text-right font-medium">Failed</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Median wait</th>
            </tr></thead>
            <tbody className="divide-y divide-line">
              {a.runners.length === 0 && <tr><td colSpan={5}><EmptyLine>No runners yet.</EmptyLine></td></tr>}
              {a.runners.map((r) => <tr key={r.runnerKey ?? `${r.machineKey}:${r.name}`}>
                <td className="px-4 py-2.5">
                  {r.runnerKey && listed.has(r.runnerKey)
                    ? <button className="font-mono text-[12px] font-medium hover:underline" onClick={() => select({ kind: "runner", key: r.runnerKey! })}>{r.name}</button>
                    : <span className="font-mono text-[12px] font-medium">{r.name}</span>}
                  <div className="text-[11.5px] text-ink-3">{r.machineName ?? "Not linked to a machine"}</div>
                </td>
                <td className="w-[30%] px-3 py-2.5"><BusyMeter percent={r.busyPercent} /></td>
                <td className="px-3 py-2.5 text-right tabular">{r.totals.jobs}</td>
                <td className={`px-3 py-2.5 text-right tabular ${r.totals.failed ? "text-bad" : ""}`}>{r.totals.failed}</td>
                <td className="px-3 py-2.5 text-right tabular">{formatDuration(r.totals.queueP50Secs)}</td>
              </tr>)}
            </tbody>
          </table>
        </Card>
      </Section>

      <div className="grid grid-cols-1 gap-x-6 min-[1000px]:grid-cols-2">
        <Section title="Top repositories" action={<span className="text-[12px] text-ink-2">by runner time</span>}><ShareBars rows={a.repos} empty="No jobs in this window." /></Section>
        <Section title="Top workflows" action={<span className="text-[12px] text-ink-2">by runner time</span>}><ShareBars rows={a.workflows} showRepo empty="No jobs in this window." /></Section>
      </div>

      <Section title="Queue wait by labels" action={<span className="text-[12px] text-ink-2">from creation until a runner started the job</span>}>
        <Card className="overflow-x-auto">
          <table className="w-full min-w-[560px] text-[13px]">
            <thead className="border-b border-line bg-sunken/40 text-left text-[12px] text-ink-2"><tr>
              <th scope="col" className="px-4 py-2 font-medium">Labels</th><th scope="col" className="px-3 py-2 text-right font-medium">Jobs</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Median</th><th scope="col" className="px-3 py-2 text-right font-medium">p95</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Longest</th>
            </tr></thead>
            <tbody className="divide-y divide-line">
              {a.labelSets.length === 0 && <tr><td colSpan={5}><EmptyLine>No queue waits recorded.</EmptyLine></td></tr>}
              {a.labelSets.map((l) => <tr key={l.labels.join(",")}>
                <td className="px-4 py-2.5"><div className="flex flex-wrap gap-1">{l.labels.map((x) => <Label key={x}>{x}</Label>)}</div></td>
                <td className="px-3 py-2.5 text-right tabular">{l.jobs}</td>
                <td className="px-3 py-2.5 text-right tabular">{formatDuration(l.p50Secs)}</td>
                <td className={`px-3 py-2.5 text-right tabular ${l.p95Secs >= 600 ? "text-warn" : ""}`}>{formatDuration(l.p95Secs)}</td>
                <td className="px-3 py-2.5 text-right tabular">{formatDuration(l.maxSecs)}</td>
              </tr>)}
            </tbody>
          </table>
        </Card>
      </Section>

      <Section title="GitHub-hosted equivalent" action={<Pill tone="info">Estimate</Pill>}>
        <Card className="overflow-hidden">
          <dl className="divide-y divide-line text-[13px]">
            {a.hostedCost.lines.map((l) => <div key={l.os} className="grid grid-cols-[1fr_auto_auto] items-baseline gap-4 px-4 py-2">
              <dt>{osLabel[l.os]} <span className="text-[12px] text-ink-2">· {plural(l.jobs, "job")}, {l.minutes.toLocaleString("en-US")} min at ${l.rate}/min</span></dt>
              <dd className="tabular">{formatUsd(l.usd)}</dd><span />
            </div>)}
            <div className="grid grid-cols-[1fr_auto_auto] items-baseline gap-4 bg-sunken/40 px-4 py-2 font-medium"><dt>Total for {window.long}</dt><dd className="tabular">{formatUsd(a.hostedCost.usd)}</dd><span /></div>
          </dl>
          <p className="border-t border-line px-4 py-2 text-[12px] text-ink-2">
            What these jobs would cost on GitHub-hosted standard runners at the per-minute rates as of {a.hostedCost.rates.asOf}, rounding each job up to whole minutes.
            It ignores included minutes, runner size and speed differences.
            {a.hostedCost.unpricedJobs > 0 && ` ${plural(a.hostedCost.unpricedJobs, "job")} without a known operating system ${a.hostedCost.unpricedJobs === 1 ? "is" : "are"} left out.`}
            {isTauri && <> Edit the rates in <button className="text-accent hover:underline" onClick={() => emitEvent("open-settings")}>Settings ▸ Activity</button>.</>}
          </p>
        </Card>
      </Section>
    </>
  );
}

function BusyMeter({ percent }: { percent: number | null }) {
  return (
    <div className="flex items-center gap-2">
      <div className="min-w-0 flex-1">{percent == null ? <div className="h-1 rounded-full bg-sunken" /> : <Meter fraction={percent / 100} />}</div>
      <span className="w-10 shrink-0 text-right text-[12px] tabular">{formatShare(percent)}</span>
    </div>
  );
}

/** Columns of busy share per bucket; dependency-free SVG that follows the theme colors. */
export function BusyChart({ a }: { a: FleetAnalytics }) {
  const bucketSecs = a.buckets.length > 1 ? (Date.parse(a.buckets[1].start) - Date.parse(a.buckets[0].start)) / 1000 : 3600;
  const capacity = Math.max(1, a.runners.length) * bucketSecs;
  const shares = a.buckets.map((b) => Math.min(1, b.busySecs / capacity));
  const width = 100 / Math.max(1, a.buckets.length);
  const peak = Math.max(0, ...shares);
  const label = (iso: string) => a.window === "24h" ? formatClock(iso) : new Date(iso).toLocaleDateString([], { month: "short", day: "numeric" });
  return (
    <Card className="px-4 pt-3 pb-2">
      <svg role="img" aria-label={`Peak ${formatShare(peak * 100)} of runner capacity busy`} viewBox="0 0 100 40" preserveAspectRatio="none" className="h-[120px] w-full">
        {[0.25, 0.5, 0.75].map((y) => <line key={y} x1="0" x2="100" y1={40 - y * 40} y2={40 - y * 40} stroke="var(--border)" strokeWidth="0.2" vectorEffect="non-scaling-stroke" />)}
        {shares.map((s, i) => {
          const b = a.buckets[i];
          return <rect key={b.start} x={i * width + width * 0.12} width={width * 0.76} y={40 - s * 40} height={Math.max(s * 40, s > 0 ? 0.4 : 0)} rx="0.4" fill="var(--busy)" opacity={0.85}>
            <title>{`${new Date(b.start).toLocaleString()}: ${formatShare(s * 100)} busy, ${plural(b.jobs, "job")} finished`}</title>
          </rect>;
        })}
      </svg>
      <div className="mt-1 flex justify-between text-[11px] text-ink-3 tabular">
        {a.buckets.length > 0 && <><span>{label(a.buckets[0].start)}</span><span>{label(a.buckets[Math.floor(a.buckets.length / 2)].start)}</span><span>now</span></>}
      </div>
    </Card>
  );
}

function ShareBars({ rows, showRepo = false, empty }: { rows: FleetRunnerTimeShare[]; showRepo?: boolean; empty: string }) {
  const fractions = barFractions(rows.map((r) => r.runSecs));
  return (
    <Card className="divide-y divide-line overflow-hidden">
      {rows.length === 0 && <EmptyLine>{empty}</EmptyLine>}
      {rows.map((r, i) => <div key={`${r.repo}:${r.name}`} className="px-4 py-2">
        <div className="flex items-baseline gap-2 text-[13px]">
          <span className="min-w-0 flex-1 truncate" title={r.name}>{r.name}{showRepo && <span className="font-mono text-[11.5px] text-ink-3"> · {r.repo}</span>}</span>
          <span className="shrink-0 text-[12px] text-ink-2 tabular">{formatHours(r.runSecs)} · {plural(r.jobs, "job")}</span>
        </div>
        <div className="mt-1 h-1 overflow-hidden rounded-full bg-sunken"><div className="h-full rounded-full bg-busy/80" style={{ width: `${fractions[i] * 100}%` }} /></div>
      </div>)}
    </Card>
  );
}

const HISTORY_PAGE = 25;

/** Completed jobs, newest first, paged from the journal with a stable cursor. */
function JobHistory({ view }: { view: FleetView }) {
  const [pages, setPages] = useState<FleetCompletedJob[][]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [ended, setEnded] = useState(false);
  const first = useQuery({
    queryKey: ["job-history", view.source, view.activity.recent[0]?.id ?? null],
    queryFn: () => commands.getJobHistory({ machineKey: null, runnerKey: null, runnerName: null, repo: null, conclusion: null, before: null, after: null, limit: HISTORY_PAGE }),
  });
  const more = useMutation({
    mutationFn: (before: string) => commands.getJobHistory({ machineKey: null, runnerKey: null, runnerName: null, repo: null, conclusion: null, before, after: null, limit: HISTORY_PAGE }),
    onSuccess: (page) => {
      setPages((p) => [...p, page.jobs]);
      setCursor(page.nextCursor);
      setEnded(!page.hasMore);
    },
  });
  const jobs = [...(first.data?.jobs ?? []), ...pages.flat()];
  const next = pages.length ? cursor : first.data?.nextCursor ?? null;
  const hasMore = pages.length ? !ended : !!first.data?.hasMore;
  return (
    <Section title="Completed jobs" count={jobs.length || undefined}>
      {first.isError ? <ErrorText error={first.error} /> : <JobRows jobs={jobs} view={view} showRunner empty={first.isPending ? "Loading…" : "No completed jobs recorded yet. Jobs appear here when they finish on a self-hosted runner."} />}
      {more.isError && <ErrorText error={more.error} className="mt-2" />}
      {hasMore && next && <div className="mt-2"><Button small variant="ghost" disabled={more.isPending} onClick={() => more.mutate(next)}>{more.isPending ? "Loading…" : "Show older jobs"}</Button></div>}
    </Section>
  );
}
