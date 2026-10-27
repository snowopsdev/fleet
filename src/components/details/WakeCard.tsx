import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import type { FleetWakeReview } from "../../bindings/FleetWakeReview";
import type { MachineView } from "../../bindings/MachineView";
import { commands, errorText } from "../../lib/tauri";
import { Button, Callout, Card, ErrorText, Section } from "../ui";
import { Icon } from "../icons";

/** The machine can be woken: an offline inventory host with `mac_address` and `wake_via`. */
export function canWake(m: MachineView): boolean {
  const host = m.desired?.host;
  return !!host?.macAddress && !!host.wakeVia && m.tailnet?.online !== true;
}

/** "Wake…" for an offline machine: a review first, then one magic packet from its relay host. */
export function WakeMachine({ machine: m, live }: { machine: MachineView; live: boolean }) {
  const [review, setReview] = useState<FleetWakeReview | null>(null);
  const [sent, setSent] = useState<string | null>(null);
  const prepare = useMutation({ mutationFn: () => commands.wakeReview(m.id), onSuccess: (r) => { setSent(null); setReview(r); } });
  const send = useMutation({
    mutationFn: (id: string) => commands.wakeExecute(id),
    onSuccess: (message) => { setReview(null); setSent(message); },
  });
  if (!canWake(m)) return null;
  const host = m.desired!.host;
  const close = () => { setReview(null); prepare.reset(); send.reset(); };
  return (
    <Section title="Wake-on-LAN" icon={<Icon name="machine" />}>
      {!review && (
        <Card className="flex items-center gap-3 px-4 py-3 text-[12.5px]">
          <div className="min-w-0 flex-1 text-ink-2">
            Sends a magic packet for <span className="mono">{host.macAddress}</span> from {host.wakeVia}, which must be on the same network and online.
            {!live && " Only for live data in the desktop app."}
          </div>
          <Button small disabled={!live || prepare.isPending} onClick={() => prepare.mutate()}>
            {prepare.isPending ? "Reviewing…" : "Wake…"}
          </Button>
        </Card>
      )}
      {prepare.isError && <Callout tone="bad" title="Couldn't prepare the wake">{errorText(prepare.error)}</Callout>}
      {sent && (
        <Card className="mt-2 flex items-start gap-1.5 px-4 py-3 text-[12.5px] selectable">
          <span className="mt-[1px] text-ok"><Icon name="check" size={12} /></span>
          {sent}
        </Card>
      )}
      {review && (
        <Card className="px-4 py-3.5 text-[12.5px]">
          <div className="font-medium">Review: wake {review.host}</div>
          <div className="mt-1 text-ink-2">
            {review.via} broadcasts one Wake-on-LAN packet for <span className="mono">{review.macAddress}</span> to{" "}
            <span className="mono">{review.broadcast}</span> over SSH. Nothing else changes; the machine shows as online once it wakes.
            When you send it, the inventory and both machines' tailnet state are read again.
          </div>
          {review.blockers.length > 0 && (
            <ul className="mt-3 space-y-1 text-bad">
              {review.blockers.map((b) => (
                <li key={b} className="flex items-start gap-1.5 selectable">
                  <span className="mt-[1px]"><Icon name="alert" size={12} /></span>
                  {b}
                </li>
              ))}
            </ul>
          )}
          <div className="mt-3 flex items-center gap-2">
            {send.isError && <ErrorText error={send.error} className="min-w-0 flex-1" />}
            <div className="ml-auto flex gap-2">
              <Button variant="ghost" onClick={close}>Cancel</Button>
              <Button variant="primary" disabled={!review.ready || send.isPending} onClick={() => send.mutate(review.id)}>
                {send.isPending ? "Sending…" : `Wake ${review.host}`}
              </Button>
            </div>
          </div>
        </Card>
      )}
    </Section>
  );
}
