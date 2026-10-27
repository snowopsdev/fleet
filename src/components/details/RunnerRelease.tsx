import type { FleetView } from "../../bindings/FleetView";
import { releaseSummary, runnersBehind } from "../../lib/versions";
import { useUi } from "../../store";

/** The latest `actions/runner` release and which runners are behind it. */
export function RunnerReleaseLine({ view }: { view: FleetView }) {
  const select = useUi((s) => s.select);
  const summary = releaseSummary(view);
  if (!summary) return null;
  const behind = runnersBehind(view);
  const overdue = behind.some((r) => r.versionStatus?.overdue);
  return (
    <p className={`mt-1 break-words text-[12px] ${overdue ? "text-warn" : "text-ink-2"}`}>
      {summary}
      {behind.length > 0 && <>
        {" "}
        {behind.slice(0, 5).map((r, i) => <span key={r.key}>
          {i > 0 && ", "}
          <button className="underline-offset-2 hover:text-ink hover:underline" onClick={() => select({ kind: "runner", key: r.key })}>{r.name}</button>
          {` (${r.versionStatus!.current})`}
        </span>)}
        {behind.length > 5 && ` and ${behind.length - 5} more`}
      </>}
    </p>
  );
}
