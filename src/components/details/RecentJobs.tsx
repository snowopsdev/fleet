import type { FleetCompletedJob } from "../../bindings/FleetCompletedJob";
import type { FleetView } from "../../bindings/FleetView";
import { useNow } from "../../hooks";
import { conclusionStyle, formatDuration, recentJobs } from "../../lib/activity";
import { timeAgo } from "../../lib/format";
import { findMachine } from "../../lib/nav";
import { useUi } from "../../store";
import { Callout, Card, EmptyLine, Section, StatusDot } from "../ui";
import { Icon } from "../icons";
import { GitHubLink } from "./shared";

/** Completed jobs as rows: outcome, job and repository, runner, duration and age. */
export function JobRows({ jobs, view, showRunner = false, empty }: { jobs: FleetCompletedJob[]; view: FleetView; showRunner?: boolean; empty: string }) {
  const now = useNow();
  const select = useUi((s) => s.select);
  return (
    <Card className="divide-y divide-line overflow-hidden">
      {jobs.length === 0 && <EmptyLine>{empty}</EmptyLine>}
      {jobs.map((job) => {
        const style = conclusionStyle[job.conclusion];
        const machine = job.machineKey ? findMachineByKey(view, job.machineKey) : undefined;
        return (
          <div key={job.id} className="flex items-start gap-3 px-4 py-2.5">
            <span className="mt-1.5" title={style.label}><StatusDot tone={style.tone} /></span>
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-baseline gap-2 text-[13px]">
                <span className="min-w-0 truncate font-medium" title={job.name}>{job.workflowName ? `${job.workflowName} · ${job.name}` : job.name}</span>
                {job.htmlUrl && <GitHubLink url={job.htmlUrl} className="shrink-0 text-ink-2 hover:text-accent" label={`Open ${job.name} on GitHub`}><Icon name="external" size={11} /></GitHubLink>}
              </div>
              <div className="mt-0.5 truncate font-mono text-[12px] text-ink-2">
                {job.repo}{job.branch && <> · {job.branch}</>}
                {showRunner && job.runnerName && <> · {job.runnerKey
                  ? <button className="hover:underline" onClick={() => select({ kind: "runner", key: job.runnerKey! })}>{job.runnerName}</button>
                  : job.runnerName}{machine && <span className="text-ink-3"> on {machine.displayName}</span>}</>}
              </div>
            </div>
            <div className="shrink-0 text-right text-[12px] tabular">
              <div className={style.tone === "bad" ? "text-bad" : "text-ink"}>{style.label} · {formatDuration(job.runSecs)}</div>
              <div className="text-[11px] text-ink-3" title={`Completed ${new Date(job.completedAt).toLocaleString()}${job.queueSecs != null ? `; waited ${formatDuration(job.queueSecs)} for a runner` : ""}`}>
                {timeAgo(job.completedAt, now)}{job.queueSecs != null && ` · waited ${formatDuration(job.queueSecs)}`}
              </div>
            </div>
          </div>
        );
      })}
    </Card>
  );
}

function findMachineByKey(view: FleetView, key: string) {
  return [...view.machines, ...view.otherMachines].find((m) => m.monitoring.machineKey === key || m.id === key) ?? findMachine(view, key);
}

/** The latest completed jobs on a runner or machine pane. */
export function RecentJobsSection({ view, runnerKey, machineKey }: { view: FleetView; runnerKey?: string; machineKey?: string }) {
  const select = useUi((s) => s.select);
  const jobs = recentJobs(view.activity, { runnerKey, machineKey });
  return (
    <Section title="Recent jobs" count={jobs.length} action={<button className="text-[12px] text-accent hover:underline" onClick={() => select({ kind: "activity" })}>Activity</button>}>
      <JobRows jobs={jobs} view={view} showRunner={!runnerKey} empty={view.activity.historySince ? "No completed jobs recorded here yet." : "Job history starts when GitHub job data arrives."} />
      {view.activity.storageError && <Callout tone="warn" title="Job history isn't being saved">{view.activity.storageError}</Callout>}
    </Section>
  );
}
