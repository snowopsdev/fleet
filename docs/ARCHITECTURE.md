# Runner Dashboard architecture

A macOS app with a local Rust core, a React UI, and read-only monitoring over Tailscale and SSH.
Nothing is installed on the machines. A bounded local journal keeps observed events.

## Code layout

| Path | Responsibility |
| --- | --- |
| `crates/fleet` | GitHub and Tailscale reads, SSH collection, desired state, view derivation, demo data |
| `src-tauri` | Desktop commands, menus, windows, polling and GitHub Keychain storage |
| `src` | React screens and generated Rust view bindings |
| `inventory/` | The private `fleet.toml` and the tracked example; see [FLEET.md](FLEET.md) |

## Data flow

Tailscale is read every 15 seconds, GitHub runners every 20 seconds, GitHub jobs every 45 seconds,
and machines over SSH every 60 seconds after the previous round completes. Refresh Now bumps a
generation counter that every poller watches, so a poller that is mid-poll when it's pressed
polls again as soon as it finishes. SSH rounds coalesce and use at most four concurrent
sessions. Blocking remote commands run off the async runtime, with a 30-second timeout per
machine.

`machine.rs` combines the native health script with the inventory runner probe in one SSH
session. macOS uses sysctl, vm_stat, top and df; Linux uses procfs and df; Windows uses CIM.
An unavailable metric remains null. Truncated responses, invalid numbers and incomplete runner
lists fail the read. Besides `/` and the home volume (every fixed drive on Windows), it reports
the filesystem holding an inventory host's runner `root`, or the root's nearest existing
parent, which is often an external volume; the root travels quoted in the script, with `~`
expanded on the machine. Filesystems are deduplicated by mount point, and the runner root's
is marked, checked as the runner volume and named in low-disk alerts. The runner probe checks only declared and legacy paths and native service
state. No runner credential files are read.

The same SSH session includes native machine diagnostics. `machine_diagnostics.rs` interprets
selected facts from Tailscale, pmset, systemd/logind or Windows powercfg, alongside disk and runner
facts. On macOS it also reads automatic login, the console session and screen lock, the login
keychain's exit status, a count of code-signing identities and the installed Xcode versions and
license, each bounded by a short time limit with stdin closed and all within a 10-second budget
whose skipped checks stay unknown (`machine_diagnostics_macos.rs`);
declared `xcode` and `simulator_runtimes` are compared with them. Only selected Tailscale preference fields are returned; raw preferences and private state
never leave the machine. Checks distinguish pass, warning, information and unknown. An incomplete
diagnostic section becomes unknown without discarding otherwise valid health and runner data.
Diagnostics inherit the health observation's timestamp and freshness. Fleet warnings use only
fresh, connected observations, and include discovered machines without inventory entries.

The inventory chooses `ssh` or `tailscale ssh`. OpenSSH uses batch mode, trusted host keys, no
agent forwarding and no port forwarding. Settings ▸ General can turn on shared SSH connections
(off by default, `ssh_reuse.rs`): monitoring reads then add `ControlMaster=auto`,
`ControlPersist=120` and a `ControlPath` in `/tmp/runner-dashboard-<uid>`, a 0700 directory the
app refuses to use unless it's a real directory owned by the user, short enough for macOS's
104-byte socket paths. Dead masters are detected with server-alive probes; OpenSSH replaces a
socket whose master is gone, and other mux failures repeat the read once without sharing.
Turning it off deletes the sockets. Reviewed changes and `tailscale ssh` never share
connections. Scripts travel on stdin, never as long command-line
arguments; Windows scripts are a base64 line read with `[Console]::In.ReadLine()`, so nothing
waits on stdin EOF through cmd.exe. Discovered machines outside the inventory can report health
using their tailnet address and advertised Tailscale SSH, or this Mac's OpenSSH configuration;
runner paths still come from the inventory.

Each result is a timestamped observation. Errors are recorded beside the last successful read.
`derive(config, observations, plan, now, source)` produces the complete view without I/O.
Machines use `ts:<node id>` as their UI identity, or `inv:<host id>` without a tailnet match.
Inventory SSH aliases resolve through `ssh -G`. Other desktop and server machines appear under
Other machines; ignored machines and phones don't get automatic SSH reads.

The UI receives fleet-updated events after every poll and change. A five-second tick derives the
view once, records monitoring transitions from it, and pushes it (before waiting on desktop
notifications) only when its content revision (the local API's) changed, or every 30 seconds as
a heartbeat. A tick's view derived before the latest push is dropped, so it can't replace newer
data a poll pushed meanwhile. Staleness flags are content, so
they reach the UI within one tick of crossing their threshold; ages advance on the UI's clock.

## Event history and alerts

`monitoring/` evaluates transitions every five seconds and writes `monitoring.json` beside the
app settings, off the async runtime. It keeps at most 30 days and 5,000 events, and checkpoints
baselines at least once a minute. History is collected only while the app runs; it can't
reconstruct events between checks. Transitions are saved atomically before a notification can
be requested; failed writes keep notifications pending, and corrupt or newer-format files are
preserved with a visible storage error. History pages use an exclusive, monotonic event-ID
cursor.

Offline alerts require three continuous minutes of fresh tailnet observations, with a healthy
local Tailscale connection; a local Tailscale outage, stale data or a gap in observations
doesn't accrue time. Low-disk alerts require distinct SSH samples below 10% free at least 60
seconds apart, and clear at 12%. Active disk conditions are grouped into one alert per machine;
equal capacity readings share one detail line. Unknown or stale observations can't clear an
alert. OS boot-session IDs detect observed reboots; uptime jitter doesn't. Service events
describe observed state transitions and can't count restarts between samples.

Desktop notifications are opt-in and batched per monitoring round by the Tauri shell. Each new
alert and recovery is announced once, after its history is saved. A one-hour maintenance pause
suppresses delivery for that machine while keeping events and in-app alerts; an unresolved
alert first raised during the pause is announced once when it ends. Persisted baselines and
announcement state prevent duplicates after a restart. Stable inventory keys preserve history
across tailnet node replacements; a changed SSH target resets alert and boot baselines. Demo
history is fictional, generated separately, and never touches the journal or sends alerts.

## Job history and analytics

`jobs/` keeps completed self-hosted jobs in `job-history.json` beside the app settings: at most
30 days and 20,000 jobs, deduplicated by job ID. Writes are atomic and run off the async
runtime; a change that only refreshes timestamps is checkpointed at most every 10 minutes.
Corrupt or newer-format files are preserved with a visible storage error, and jobs are then
kept in memory until the app quits. History starts when the file is first created
(`historySince`); analytics never count earlier time as idle.

After each job scan, `update_job_history` follows every run seen with a queued or running
self-hosted job (persisted, so a restart doesn't lose them; dropped after six days). When a run
leaves the scan, its latest attempt is read once with
`GET /repos/{o}/{r}/actions/runs/{id}/jobs`, which also returns jobs too short to have been
seen running; jobs seen running in an earlier attempt are read with
`GET /repos/{o}/{r}/actions/jobs/{id}`. A run that's still going, or can't be read, is asked
about again after two minutes. Only jobs assigned to a runner and asking for `self-hosted` (or
run by one of the account's runners) are recorded, with queue wait (created → started), run
time (started → completed), conclusion, labels, repository, workflow, runner key and the
machine's stable history identity. At most 20 runs per account are read per round. Reads use
the client the account's polls created, skip a rate-limited account, stop with fewer than 200
requests left, and a rate-limited read sets the account's `rateLimitedUntil` like any poll.

`jobs/analytics.rs` summarizes a 24-hour, 7-day or 30-day window without I/O: busy time is
each job's run overlapping the window, and busy percent divides it by the recorded part of the
window (times the runner count for a machine). The hosted-cost estimate rounds each job up to
whole minutes at the editable rates in Settings. Analytics are read on demand (Tauri command,
`/v1/analytics`), not pushed with every fleet view; the view carries only the latest 10 jobs
per runner and machine and the job feed's state.

The optional real-time job feed (`jobs/feed.rs`, `jobs/webhook.rs`) supervises one
`gh webhook forward --org=ORG --events=workflow_job --url=URL --secret=SECRET` per organization
that turned it on. The extension creates a temporary `cli` webhook on the organization
(`admin:org_hook`) and relays deliveries over a websocket. The receiver is a separate loopback
listener on an ephemeral port: it refuses non-loopback peers and requests with an `Origin`
header, and checks `X-Hub-Signature-256` against a random per-launch secret that is never
written down (it's visible in the child's argv, which the extension requires). Deliveries
update the in-flight scan between polls and record completed jobs directly. Forwarders run in
their own process group; they restart after an exit (quickly after a healthy run, since GitHub
closes the websocket every few minutes; otherwise 5 s doubling to 5 min), retry every 10 minutes
when the extension, scope or login is missing, and are terminated when the setting is turned
off, in demo mode, and on app exit. While every scanned organization's feed is forwarding, the
45-second job scan runs only every 3 minutes. If the app is killed without its exit handler,
a forwarder can outlive it until it next disconnects.

Demo job history is fictional, deterministic on a fixed time grid, generated from the demo
view's runners, and never touches the journal.

## Status rules

- Installed/configured and service state come from SSH; connected, busy and idle come from GitHub.
  A running service doesn't prove that a runner is idle.
- Machine facts become stale after 150 seconds or a failed check. Old values remain visible with
  their original timestamp. A changed SSH target never inherits the previous target's health.
- GitHub runners become stale after 150 seconds; jobs after 300 seconds. Without fresh GitHub
  runner data, activity is unknown. Current jobs require an actual runner_id match in the jobs API.
- Elapsed time never labels a job stuck. Machines and runners sort by name, independent of status.
- Queued jobs list label-compatible runners, not a promise of scheduling; group access can also
  affect eligibility. A repository runner is compatible only with its own repository's jobs.
- `waiting`, `pending` and `requested` jobs are held by GitHub (an approval, a concurrency group)
  rather than waiting for a runner: the queue counts them as `held` instead of listing them.
- A runner's version (GitHub's, else the machine's) is compared with recent `actions/runner`
  releases. It's overdue when more than one minor release behind, on another major version, or
  missing a release for over 30 days, counted from the oldest release it lacks (not the
  latest). "Update available" findings wait a week from that release.
- Fresh SSH runner facts update displayed inventory drift. Every change still re-reads the host
  through the desired-state engine before review and again before execution.

## GitHub and local access

Monitoring uses the GitHub CLI login by default, held in memory, or a pasted token kept in the
macOS Keychain. Changes always use the `gh` login ([FLEET.md](FLEET.md#setup)). Org runner lists
and per-repository workflow jobs are fetched separately; repository scope is configurable in
Settings. Runner groups are read with the runner list, only to warn about groups with
self-hosted runners that public repositories can use; a failure to read them isn't shown as
an organization error.

- **Accounts:** an added account is an organization or a personal account. Either can list
  repositories whose repository runners (`/repos/{owner}/{repo}/actions/runners`) are shown too;
  a personal account has only those. Repository runners are monitoring only: they're never
  matched to inventory hosts, and desired state stays organization-only. Each repository's
  listing is kept with its own read time. A repository that can't be listed (renamed, deleted,
  no admin access) gets its own error and finding, keeps its last runners aging into stale, and
  doesn't affect the organization's or other repositories' runners or re-read the token. An
  organization list that can't be read, a rejected token or a rate limit fails the account's
  runner read, keeping the last good lists beside the error. A personal account's recently
  pushed repositories come from the signed-in user's list (`/user/repos`, filtered to the
  account), so sign in as its owner or a collaborator.
- **Clients:** each account keeps one client and its ETag cache across polls. After a 401 or
  403 the token is read again before the next poll, at most every two minutes unless Refresh
  Now asks, and swapped into the same client so conditional requests keep working. A client
  installed by a sign-in in Settings while a token was being re-read is kept.
- **Rate limits:** a 403 or 429 with `x-ratelimit-remaining: 0`, a `retry-after` header or a
  rate-limit message is a rate limit, not a permission error. The account's polls are skipped
  until GitHub's reset (secondary limits: `retry-after`, else a minute) plus 1-5 seconds of
  jitter; `rateLimitedUntil` shows it. Runner and job polls share the pause: another outcome
  lifts it only if that poll started after the limit was observed.
- **Job scope:** a "recent" scope scans the most recently pushed repositories, plus the
  account's runner repositories and repositories where self-hosted jobs ran in the last week.
  While fresh data shows a busy runner without a linked job, each scan also probes the next 10
  repositories by name, so scheduled and manual runs in quiet repositories are found; the probe
  moves to the next page only after its page was scanned. Everything stays within the
  50-repository cap: up to 20 places are reserved for these extras by scanning fewer recently
  pushed repositories, and the scan is marked incomplete when either gives way. Runs with
  status `in_progress`, `queued`, `waiting`, `pending` and `requested` are listed.
- **Runner releases:** the latest 30 `actions/runner` releases (no drafts or pre-releases) are
  read at most hourly, conditionally, through a github.com account (never GitHub Enterprise
  Server).

The local HTTP API ([HTTP_API.md](HTTP_API.md)) captures one fleet snapshot per request off
the async runtime and adds provenance and a content-based revision to the response. The shared
`issues` derivation feeds both the desktop's attention rows and `/v1/issues`; active alerts
replace duplicate disk and offline rows. The OpenAPI 3.1 contract derives view schemas from the
Rust types. `POST /v1/checks` runs the same read-only refresh, inventory check and runner checks
as the app, waiting for automatic checks already in flight; the regular poller still coalesces
redundant refreshes.

Demo data passes through the same derivation. Demo mode disables live polling and runner changes.
Tests replace GitHub, SSH and service boundaries; they never contact real hosts or the org.

## Desired state (Runner Fleet, merged)

`crates/fleet/src/desired/` ports Runner Fleet's Python (`model`, `inventory`, `github`,
`remote`, `state`, `plan`, `operations`), with its tests in `desired/tests.rs`. It keeps the
file format, rules and messages, including `exclude_labels` and the `windows-logon` service mode
(`desired/windows_logon.rs`), where the runner runs in an auto-logged-on desktop session for GUI
tests. It differs in the hardening listed below, and in additions Runner Fleet doesn't have:
the `darwin-x64` and `linux-arm64` platforms and optional host keys for toolchain drift and
Wake-on-LAN ([FLEET.md](FLEET.md#platforms)). The machine scripts
(`crates/fleet/scripts/`) are embedded with `include_str!`, and their RF_* values travel in the
script on SSH stdin.

- **Boundaries:** `CommandRunner` runs `gh`, `ssh` and `tailscale`. `FleetGithub` and
  `FleetRemote` are traits, so tests fake GitHub and hosts and never contact either.
- **Pure core:** `state::build_status_rows` and `plan::generate_plan` are pure functions of
  (inventory, snapshot). An unreachable GitHub or host becomes "unknown"/`inspect`, never
  "missing".
- **Guards kept from Runner Fleet:**
  - No `--replace`; configured runner directories are never moved or deleted.
  - A name collision or orphan refuses apply.
  - A busy runner is never restarted or removed.
  - Retire needs an online, idle, labelled, in-group, active replacement.
  - Decommission needs `decommission = true`.
  - A migrating host applies one runner at a time.
  - The runner group policy is validated before registering.
- **Hardening:** OpenSSH runs with `StrictHostKeyChecking=yes`, `ForwardAgent=no` and
  `ClearAllForwardings=yes`. Passwords come from the review form, not environment variables,
  held in a `Transient` whose `Debug` hides the value.
- **Engine (`engine_desired.rs`):**
  - `desired_refresh`/`desired_doctor`/`desired_logs` are read-only.
  - `desired_review` re-reads one host and returns a `ReviewView`.
  - `desired_execute` re-reads it again, and refuses unless the regenerated plan items equal
    the reviewed ones. Reviews are single-use and expire after 15 minutes.
  - SSH aliases are resolved off the async runtime so inventory hosts join their tailnet
    machines.
- **Parity:** `examples/fleet.rs` prints Runner Fleet's `--json` shapes for
  [the parity script](FLEET.md#parity-with-runner-fleet).
- **Demo:** `demo_desired.rs` is a fictional inventory and snapshot run through the same view
  builder.
- **Wake-on-LAN (`engine_wake.rs`, `wake.rs`):** like a change, a wake is reviewed first and
  single-use; sending re-reads the inventory and the view and refuses if the address, the
  `wake_via` host or their tailnet state changed. `WakeSender` is the boundary tests fake.
