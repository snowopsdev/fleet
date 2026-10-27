# Local HTTP API

While the desktop app runs, it serves a read-only JSON API on `http://127.0.0.1:47622`, for
scripts and AI agents on this Mac. Set `RUNNER_DASHBOARD_API_PORT` to change the port, or `0` to
pick a free one. The app writes the URL it bound to
`~/Library/Application Support/dev.runnerdashboard.app/local-api.json`. Turning the API off in
Settings ▸ General takes effect the next time the app starts.

`GET /` lists routes, capabilities, limits and interpretation rules. `GET /v1/openapi.json` is
the OpenAPI 3.1 contract, including JSON Schemas generated from the Rust view types.

## Security

Every route requires a loopback client address and a loopback `Host` header, and rejects any
request with an `Origin` header, so browser pages can't call it. No CORS headers are sent. Only
GET, HEAD and `POST /v1/checks` are accepted. Responses never include tokens or other
credentials; log reads return runner diagnostic output as written by the runner. No endpoint
installs, registers, restarts, retires, decommissions, cleans up, upgrades, drains or resumes
runners, mints runner tokens, wakes machines, changes alert destinations or automatic actions,
or changes host configuration.

## Routes

| Route | Filters and result |
| --- | --- |
| `GET /` | Capabilities, limits and semantics |
| `GET /v1/fleet` | Complete FleetView, including `otherMachines`, `issues` and `automation` |
| `GET /v1/summary` | Summary, organization and tailnet status |
| `GET /v1/machines` | `scope=all` (default), `fleet` or `discovered`; `os`, `connection`, `q`, `after`, `limit` |
| `GET /v1/machines/{id}` | One machine |
| `GET /v1/runners` | `machine`, `org`, `activity`, `label`, `repo`, `after`, `limit` |
| `GET /v1/runners/{key}` | One runner; optional `org` |
| `GET /v1/queue` | `org`, `repo`, `label`, `after`, `limit` |
| `GET /v1/discovered` | Other machines and ignored machines |
| `GET /v1/desired` | Global `plan` and `hosts`; optional `machine` |
| `GET /v1/monitoring` | Alerts and latest 50 events; optional `machine` |
| `GET /v1/events` | History: `machine` or `machineKey`, `before`, `limit` ([History](#history)) |
| `GET /v1/issues` | `machine`, `category`, `stale`, `muted`, `after`, `limit` |
| `GET /v1/changes` | `since=<revision>`; returns `changed` ([Polling](#polling-and-errors)) |
| `GET /v1/jobs/history` | `machine`, `runner`, `org`, `repo`, `conclusion`, `before` or `after`, `limit` ([Jobs](#job-history-and-analytics)) |
| `GET /v1/analytics` | `window=24h` (default), `7d` or `30d` ([Jobs](#job-history-and-analytics)) |
| `GET /v1/logs` | `runner`, `org`, `lines` ([Logs](#fresh-checks-and-logs)) |
| `POST /v1/checks` | Start a read-only check ([Checks](#fresh-checks-and-logs)) |
| `GET /v1/checks/{id}` | Check status and result |
| `GET /v1/openapi.json` | OpenAPI 3.1 contract |

HEAD works on every GET route and returns headers only.

## Responses and identity

Successful JSON responses carry the route's fields plus `apiVersion: 1`, `source` (`live` or
`demo`), `generatedAt` and `revision` at the top level. Every response that reads a snapshot
also has `X-Api-Version`, `X-Data-Source` and `X-Fleet-Revision` headers; the OpenAPI document
carries its provenance only in these headers.

`generatedAt` is when the snapshot was assembled. Individual `asOf`, `checkedAt` and `stale`
fields describe observation freshness. Missing or stale data doesn't mean zero, idle, healthy
or recovered. `source: demo` identifies fictional observations.

Machine identifiers are resolved in this order: exact `id`, inventory identity (`inv:<host>`
or the inventory host ID), then case-insensitive display name, host or tailnet hostname.
Runner lookup prefers the exact `key`, then a case-insensitive name; `org` can disambiguate
names. Ambiguity returns 409 with `candidates` containing exact IDs. URL-encode IDs and runner
keys, including embedded paths. Prefer stable inventory identities for machine-scoped queries.

## Filters and pagination

Unknown query fields and invalid enum values are errors. The enums are:

| Filter | Values |
| --- | --- |
| `os` | `macos`, `linux`, `windows`, `other` |
| `connection` | `connected`, `stale`, `offline`, `error`, `disabled`, `pending` |
| `activity` | `busy`, `idle`, `offline`, `unknown` (busy and idle need fresh GitHub data) |
| `category` | `alert`, `diagnostic`, `inventory`, `connection`, `runner`, `queue`, `github`, `tailnet`, `storage`, `operation` |

`q` is a case-insensitive substring of a machine's ID, display name or host. Runner `label`
matches observed GitHub labels; `repo` matches the runner's linked current job, not repository
access permissions. GitHub data freshness is visible on each runner.

`/v1/machines` includes fleet machines and other discovered machines by default; ignored
machines are in `/v1/discovered`. Summary machine counts refer to fleet machines. Queue scope
metadata (`complete`, `asOf`, `otherQueued`) stays fleet-wide when jobs are filtered.

`/v1/desired` returns each selected host's declaration, plan, blocked reasons, errors, check
timestamps, runner check findings and per-runner desired status. Its global `plan` stays
fleet-wide.

Runners and organizations carry GitHub details for monitoring: `github.repo` is set on
repository runners (monitoring only, never matched to inventory hosts), `versionStatus` compares
the runner's version with the latest release in the fleet's `runnerRelease`, and
`orgs[].rateLimitedUntil` is set while GitHub polls for that account are paused.
`orgs[].runnerRepoErrors` lists runner repositories that couldn't be listed; their runners keep
their last listing and age into stale. `versionStatus.missingSince` is when the oldest release
the runner lacks was published; `overdue` counts from it. The queue's
`held` counts self-hosted jobs GitHub holds for an approval or concurrency group.

`/v1/fleet`'s `automation` describes reviewed drains (`draining` or `stopped`), the watchdog
(whether it's on, and each runner's recent attempts, next attempt and give-up), alert
destinations (kind, name, host summary, queue length and last outcome; never URLs or tokens)
and the published availability variable. It is read-only: drains, the watchdog and
destinations are changed only in the app. A watchdog give-up is an alert (`watchdogGaveUp`) in
`/v1/monitoring` and `/v1/issues`.

`/v1/issues` is the desktop's attention feed. `summary.attention` equals the full
`issues.length`, including unresolved stale or muted alerts. The UI renders alerts, history
storage failures and operation failures in their own panels; the remaining findings appear
under Needs attention. IDs and entity references support navigation and deduplication.

Lists default to 100 entries and accept `limit=1..200`. Each page has `total` (the full filtered
count) and `nextCursor` (null on the last page). Pass it as `after` on the next request with the
same filters. Lists sort by stable ID; queue cursors contain organization and job ID. These are
live reads, not pinned snapshots: restart list pagination if `revision` changes.

## History

```sh
curl -s 'localhost:47622/v1/events?machine=inv:studio&limit=100'
curl -s 'localhost:47622/v1/events?machineKey=inv:removed-host&before=00000000000000000123'
```

Use either `machine` (resolved against current machines) or `machineKey` (a retained history
identity, including a removed machine). Omit both for all machines. The default limit is 50;
the maximum is 200. Events are newest first. Pass `nextCursor` as exclusive `before`;
`hasMore` and `nextCursor: null` identify the last page. Cursors are 20-digit event IDs.
New events don't move an existing history cursor. Old entries can expire during pagination.

History contains transitions observed while the app was running, within the retention limits
in [ARCHITECTURE.md](ARCHITECTURE.md#event-history-and-alerts). Completed jobs are in
[`/v1/jobs/history`](#job-history-and-analytics). Demo history is separate from the live journal.

## Job history and analytics

```sh
curl -s 'localhost:47622/v1/jobs/history?machine=inv:studio&conclusion=failure&limit=50'
curl -s 'localhost:47622/v1/jobs/history?after=0000000176073920000000000000000000000111'
curl -s 'localhost:47622/v1/analytics?window=7d'
```

`/v1/jobs/history` returns completed self-hosted jobs recorded on this Mac: `jobs`, `order`,
`hasMore`, `nextCursor`, `historySince`, `storageError`, `retentionDays` and `maxJobs`. Each job
has its `conclusion` (`success`, `failure`, `cancelled`, `skipped`, `timedOut`, `actionRequired`,
`neutral`, `stale`, `startupFailure`, `unknown`), `queueSecs`, `runSecs`, `runnerKey`,
`machineKey` and `source` (`poll`, `webhook` or `demo`).

- `machine` resolves like other routes; an ID that matches no current machine is used as a
  retained history identity, so removed machines keep their jobs. `runner` takes a runner key
  or name (`org` disambiguates); a name GitHub no longer lists matches recorded runner names.
  `repo` is `owner/name` or the bare name, case-insensitive.
- Cursors are 40 digits (completion time in milliseconds, then job ID). Without `after`, jobs
  are newest first and `nextCursor` goes in exclusive `before`. With only `after`, jobs are
  oldest first and `nextCursor` goes in exclusive `after`, which suits polling for new
  completions. The default limit is 50; the maximum is 200.

`/v1/analytics` returns `analytics` for the window ending at `generatedAt`: `from`, `to`,
`observedFrom` (start of recorded time in the window), `totals`, per-`machines` and
per-`runners` busy seconds and `busyPercent`, `labelSets` (queue-wait median, p95 and maximum
per label set, slowest first), the top 10 `repos` and `workflows` by runner time, `buckets` for
a chart (hourly, six-hourly or daily), and `hostedCost`, an estimate at the rates in Settings.
`busyPercent` is null with less than a minute of recorded time. Both routes read only local
data; they never call GitHub. Jobs that started and finished between two scans can be missing
unless the real-time job feed is on. In demo mode both return fictional data.

The job feed's webhook receiver is a separate loopback listener, not part of this API.

## Fresh checks and logs

```sh
curl -si -X POST localhost:47622/v1/checks \
  -H 'Content-Type: application/json' \
  -d '{"kind":"doctor","machine":"inv:studio"}'
# Poll the Location header returned with HTTP 202:
curl -s localhost:47622/v1/checks/RETURNED_ID
curl -s 'localhost:47622/v1/logs?runner=studio-01&lines=200'
```

Check kinds:

- `refresh`: refresh Tailscale and SSH observations. Without `machine`, also refresh watched
  GitHub runners and job scans. A machine-scoped request refreshes SSH for that machine only.
- `inventory`: read GitHub and SSH facts and compare them with the inventory, for all enabled
  inventory machines or the specified inventory machine.
- `doctor`: run the runner checks (read-only prerequisite and configuration checks) for all
  enabled inventory machines or the specified inventory machine.

The response is `202` with `check.id` and `Location`. Poll until status is `completed` or
`failed`. `completed` means collection finished, not that the fleet is healthy: inspect
`result.errors`, `result.issues` and `result.skipped`, as well as per-host observations.
`failed` includes a structured `check.error`. Results carry their own source, timestamp and
revision so they remain interpretable after later polling or a mode change.

Only one HTTP check runs at a time; overlapping requests receive 429 and `Retry-After`. A job
continues after its client disconnects, and waits for overlapping automatic checks before
collecting its requested scope. The registry keeps at most 100 jobs for one hour from their
start; running jobs stay available. Restarting the app clears it. POST bodies are limited to
16 KiB.

Logs require `runner` (key or unambiguous name), optional `org`, and `lines=1..10000`
(default 200). The runner must be declared in the inventory or be its retained legacy runner.
The response contains the last diagnostic log's `text`, `runnerKey`, `machineKey`, `readAt`,
`requestedLines` and `truncated`. Text keeps at most the last 1 MiB, cut at a UTF-8 boundary.
Two log reads may run at once. Arbitrary file paths are not accepted. Checks and logs return
`demo_mode` while demo data is showing.

## Polling and errors

`revision` is an opaque fingerprint of the fleet snapshot excluding `generatedAt`. It changes
when observations, settings or derived freshness change. The desktop app uses the same revision
to skip pushing unchanged views to its window. `GET /v1/changes?since=<revision>`
returns `changed`. Alternatively, keep the `ETag` from a GET and send `If-None-Match` to the
same URL; unchanged representations return 304 with no body. ETags apply to the response,
including its filters, pagination and job state. Demo timestamps advance with each generated
snapshot, so demo revisions can change on every read.

Error bodies contain `code`, a human-readable `error`, `retryable` and `candidates`. Errors from
accepted requests also carry the provenance fields; client address, Host, Origin and method
rejections are returned before a snapshot is read and carry none.

| Code | HTTP status | Meaning |
| --- | --- | --- |
| `invalid_request` | 400, 413, 415 or 422 | Invalid query, JSON, content type or body size |
| `not_found` | 404 | Unknown endpoint, entity or expired check |
| `ambiguous_identifier` | 409 | Use an exact ID from `candidates` |
| `demo_mode` | 409 | Turn off demo data for live troubleshooting |
| `unsupported_target` | 422 | Target can't support this check or log read |
| `busy` | 429 | Retry after the `Retry-After` delay |
| `forbidden` | 403 | Client address, Host or Origin rejected |
| `method_not_allowed` | 405 | Only GET/HEAD and POST /v1/checks are supported |
| `check_failed` | 502 or failed job | A read-only operation failed |
| `internal` | 500 | Snapshot or response processing failed |
