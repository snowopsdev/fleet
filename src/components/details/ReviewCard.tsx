import { useMutation } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import type { ChangeRequest } from "../../bindings/ChangeRequest";
import type { FleetCleanupTarget } from "../../bindings/FleetCleanupTarget";
import type { FleetPlanItem } from "../../bindings/FleetPlanItem";
import type { ReviewView } from "../../bindings/ReviewView";
import { formatKib, requestTitle } from "../../lib/desired";
import { commands } from "../../lib/tauri";
import { Button, Callout, Card, ErrorText, Section } from "../ui";
import { Icon } from "../icons";
import { PlanItems, type Change } from "./changes";

/** The review (or its preparation) for a change opened with `useChange`. */
export function ChangePanel({ change, busy }: { change: Change; busy: boolean }) {
  if (change.preparing) {
    return <Card className="mt-4 px-4 py-3 text-[12.5px] text-ink-2">Reading the machine to prepare the review…</Card>;
  }
  if (change.error) return <Callout tone="bad" title="Couldn't prepare the review">{change.error}</Callout>;
  if (!change.review) return null;
  return <ReviewCard review={change.review} busy={busy} onClose={change.close} reopen={change.open} />;
}

function ReviewCard({ review: r, busy, onClose, reopen }: { review: ReviewView; busy: boolean; onClose: () => void; reopen: (request: ChangeRequest) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const [servicePassword, setServicePassword] = useState("");
  const [sudoPassword, setSudoPassword] = useState("");
  const [phrase, setPhrase] = useState("");
  const run = useMutation({
    mutationFn: () =>
      commands.desiredExecute(
        r.id,
        { servicePassword: servicePassword || null, sudoPassword: sudoPassword || null },
        r.confirmPhrase ? phrase : null,
      ),
    onSuccess: onClose,
  });
  useEffect(() => {
    ref.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [r.id]);
  const dangerous = r.request.kind === "retire" || r.request.kind === "decommission" || r.request.kind === "cleanup";
  // The first `cleanup.length` items describe the cleanup targets, in order.
  const targets = new Map<FleetPlanItem, FleetCleanupTarget>(r.cleanup.map((t, n) => [r.items[n], t]));
  const ready =
    r.ready &&
    !busy &&
    (!r.needsServicePassword || servicePassword.length > 0) &&
    (!r.confirmPhrase || phrase.trim() === r.confirmPhrase);

  return (
    <div ref={ref}>
      <Section title={`Review: ${r.title}`} icon={<Icon name="plan" />}>
        <Card className="px-4 py-3.5 text-[12.5px]">
          <div className="text-ink-2">
            Read from the machine just now. When you run it, the machine is read again and nothing happens if anything changed.
          </div>
          <ChangeNotes review={r} />
          {r.request.kind === "cleanup" && <CleanupOptions request={r.request} reopen={reopen} disabled={busy} />}
          {r.items.length > 0 && (
            <div className="mt-3">
              <PlanItems items={r.items} action={(i) => targets.has(i) ? <TargetSize target={targets.get(i)!} /> : null} />
            </div>
          )}
          {r.request.kind === "cleanup" && r.cleanup.length > 0 && (
            <div className="mt-2 text-[11.5px] text-ink-3">
              Total found: {formatKib(r.cleanup.reduce((sum, t) => sum + (t.kib ?? 0), 0))}. Sizes are from this review; the machine is checked again before anything is removed.
            </div>
          )}
          {r.blockers.length > 0 && (
            <ul className="mt-3 space-y-1 text-bad">
              {r.blockers.map((b) => (
                <li key={b} className="flex items-start gap-1.5 selectable">
                  <span className="mt-[1px]">
                    <Icon name="alert" size={12} />
                  </span>
                  {b}
                </li>
              ))}
            </ul>
          )}

          {r.ready && (
            <div className="mt-3 space-y-2">
              {r.needsServicePassword && (
                <Field label={`Password for ${r.needsServicePassword}`} hint="Needed to register the Windows service. Used once, never stored.">
                  <input
                    type="password"
                    className="input w-[260px]"
                    autoComplete="off"
                    value={servicePassword}
                    onChange={(e) => setServicePassword(e.target.value)}
                  />
                </Field>
              )}
              {r.acceptsSudoPassword && (
                <Field label="Sudo password (optional)" hint="Only if this machine lacks non-interactive sudo. Used once, never stored.">
                  <input
                    type="password"
                    className="input w-[260px]"
                    autoComplete="off"
                    value={sudoPassword}
                    onChange={(e) => setSudoPassword(e.target.value)}
                  />
                </Field>
              )}
              {r.confirmPhrase && (
                <Field label={`Type “${r.confirmPhrase}” to confirm`} hint="Stops and deregisters the exact runner; its directory is kept.">
                  <input className="input mono w-[260px]" autoComplete="off" value={phrase} onChange={(e) => setPhrase(e.target.value)} />
                </Field>
              )}
            </div>
          )}

          <div className="mt-3 flex items-center gap-2">
            {run.isError && <ErrorText error={run.error} className="min-w-0 flex-1" />}
            <div className="ml-auto flex gap-2">
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button variant={dangerous ? "danger" : "primary"} disabled={!ready || run.isPending} onClick={() => run.mutate()}>
                {run.isPending ? "Re-checking…" : requestTitle(r.request)}
              </Button>
            </div>
          </div>
        </Card>
      </Section>
    </div>
  );
}

function Field({ label, hint, children }: { label: string; hint: string; children: React.ReactNode }) {
  return (
    <label className="flex items-center gap-3">
      <span className="w-[200px] shrink-0 text-ink-2">{label}</span>
      {children}
      <span className="text-[11.5px] text-ink-3">{hint}</span>
    </label>
  );
}

/** What a maintenance change does, in plain words. */
function ChangeNotes({ review: r }: { review: ReviewView }) {
  const note = (() => {
    switch (r.request.kind) {
      case "cleanup":
        return "Removes only the exact paths below, each resolved on the machine and checked to be inside the runner directory (or your home folder) with no symlink on the way. A runner that is busy, or becomes busy, is left alone; .runner, credentials, configuration, bin and externals are never touched.";
      case "upgrade":
        return `Upgrades to ${r.upgradeTo ?? "the latest release"}, one runner at a time: stops the idle service, downloads the release on the machine and verifies GitHub's SHA-256 digest, replaces bin/ and externals/ in place (the previous files are kept as bin.fleet-previous and externals.fleet-previous), then starts it and checks its version. A failure leaves that runner stopped.`;
      case "drain":
        return "GitHub has no way to pause a runner, so draining stops the runner's service on the machine: now if it's idle, otherwise once its current job finishes (the app keeps the drain and checks every 20 seconds). It shows offline on GitHub until you resume it. You can cancel a drain until it stops the service.";
      case "resume":
        return "Starts the runner's service again so it can take jobs.";
      default:
        return null;
    }
  })();
  return note ? <div className="mt-2 text-ink-2">{note}</div> : null;
}

function TargetSize({ target: t }: { target: FleetCleanupTarget }) {
  const parts = [
    t.kib != null ? formatKib(t.kib) : null,
    t.count != null ? (t.kind === "simulators" ? `${t.count} simulators` : t.kind === "diag" ? `${t.count} logs` : `${t.count}`) : null,
    t.detail,
  ].filter(Boolean);
  return parts.length ? <span className="shrink-0 font-mono text-[11.5px] text-ink-2 tabular">{parts.join(" · ")}</span> : null;
}

/** The cleanup's opt-in choices. Changing one reviews again. */
function CleanupOptions({ request, reopen, disabled }: { request: Extract<ChangeRequest, { kind: "cleanup" }>; reopen: (request: ChangeRequest) => void; disabled: boolean }) {
  const { options } = request;
  return (
    <div className="mt-3 space-y-1.5">
      <label className="flex items-center gap-2 text-ink-2">
        <input type="checkbox" checked={options.wholeWork} disabled={disabled} onChange={() => reopen({ ...request, options: { ...options, wholeWork: !options.wholeWork } })} />
        Empty each idle runner's whole _work (workspaces, tool cache and temp files), not only _temp and _actions
      </label>
      <label className="flex items-center gap-2 text-ink-2">
        Remove diagnostic logs older than
        <select className="input w-[90px]" value={options.diagOlderThanDays} disabled={disabled} onChange={(e) => reopen({ ...request, options: { ...options, diagOlderThanDays: Number(e.target.value) } })}>
          {[7, 14, 30, 90].map((d) => <option key={d} value={d}>{d} days</option>)}
        </select>
      </label>
    </div>
  );
}
