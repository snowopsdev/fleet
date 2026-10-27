# Runner Dashboard

A macOS app for watching GitHub Actions self-hosted runners across the machines on your
Tailscale tailnet. It answers three questions per runner: is it installed, is it connected to
GitHub, and is it doing work right now?

**Personal project:** This is completely vibe coded for my own personal use case. I won't be
accepting feature requests, but you're welcome to fork it and make it your own.

![Runner Dashboard fleet overview](docs/screenshot.png)

- **Mac app** (Tauri 2 + React). Machines come from your tailnet. The sidebar lists each runner
  machine with its runners nested under it, below a fleet overview. A machine's pane shows its
  status, SSH health, diagnostics, inventory configuration, runner checks and pending changes.
  A runner's pane shows its GitHub and service state, its jobs, and how it compares with the
  inventory.
- **SSH monitoring** reads CPU, load, memory, free disk, uptime and runner configuration from
  the machines. Nothing is installed on the machines.
- **Event history and alerts** keep observed connections, reboots and runner service changes
  on this Mac, with offline and low-disk alerts, optional desktop notifications, and optional
  Slack, Discord, ntfy or JSON webhook destinations.
- **Maintenance:** reviewed disk cleanup, runner upgrades, and draining a runner after its
  current job; an opt-in watchdog that restarts stopped runner services; and an opt-in
  organization variable that lets workflows fall back to GitHub-hosted runners.
- **Job history and Activity** keep completed self-hosted jobs on this Mac for 30 days. The
  Activity page shows busy time per machine and runner, job counts and success rates, queue
  waits per label set, the repositories and workflows using the most runner time, and an
  estimate of what the same jobs would cost on GitHub-hosted runners. Runner and machine panes
  list their latest jobs. An optional real-time job feed uses `gh webhook forward`.

Monitoring is read-only. Changes go through Runner Fleet's desired-state rules, ported to Rust:
the app installs, reconciles, retires, decommissions, restarts, cleans up, upgrades, drains or
resumes runners only through reviewed plans against your inventory, `inventory/fleet.toml`.
The two exceptions are off by default and turned on in Settings: watchdog restarts and the
availability variable ([docs/FLEET.md](docs/FLEET.md#automatic-actions-opt-in)).

## Requirements

- macOS, with its built-in OpenSSH.
- Tailscale, signed in to your tailnet. The app reads machines through the Tailscale CLI.
- The GitHub CLI (`gh`), logged in as an organization owner. Monitoring can use a pasted token
  instead; changes always use the `gh` login.
- Rust 1.93+, Node 22 and pnpm, to build.
- python3, only for `scripts/fleet-parity.sh`.

## Quick start

```sh
pnpm install
pnpm tauri dev          # desktop app (Vite on 127.0.0.1:5487)
pnpm dev                # browser-only preview with the demo fixture
```

Tests, lints, generated bindings and fixtures: see [AGENTS.md](AGENTS.md).

## Demo data

**View ▸ Toggle Demo Data** (⇧⌘D), or the button on the empty state, swaps in a fictional
fleet (`acme-demo`, `*.demo.ts.net`) that runs through the same derivation as live data. A
**DEMO DATA** banner shows while it's on, and live polling pauses. The browser-only preview
always uses demo data.

## First live setup

1. Create your inventory: see [Setup](docs/FLEET.md#setup) in the operating guide. The browser
   preview and tests work without one.
2. **Settings (⌘,) ▸ GitHub ▸ Add organization…**. By default it uses your GitHub CLI login:
   the app asks `gh` for its token when it needs one and keeps it in memory only. Listing an
   organization's runners needs an organization owner and the `admin:org` scope. If your login
   lacks the scope, Settings shows the fix:
   `gh auth refresh --hostname github.com --scopes admin:org`. For monitoring, you can paste a
   fine-grained token instead (organization permission **Self-hosted runners: Read**,
   repository permission **Actions: Read**), which is stored in your Keychain. Access is checked
   before anything is saved.

   A **personal account** can't have organization runners: add it as a personal account and
   list the repositories whose runners to show. An organization can list repositories too, for
   runners registered to a repository. Listing a repository's runners needs admin access to it
   (fine-grained **Administration: Read**; gh and classic tokens: the `repo` scope). A
   repository that can't be listed is reported on its own, without hiding other runners. For a
   personal account, sign in as its owner (or a collaborator): recently pushed repositories come
   from the signed-in user's repository list. Personal accounts and repository runners are
   monitored only; inventory changes apply to organization runners.
3. Machines come from your tailnet; there's no Add step. Machines in the inventory or with
   runners on GitHub are listed under **Machines**. Other desktop and server machines sit in a
   collapsed **Other machines** group, where you can **Ignore** them (reversible under
   **Ignored**). Phones and TVs are hidden.
4. Make each machine reachable through your existing SSH setup. Inventory hosts use their `ssh`
   target and `ssh_transport`; other machines use their tailnet address and advertised
   Tailscale SSH, falling back to this Mac's OpenSSH configuration. OpenSSH host keys must
   already be trusted; Tailscale SSH uses the tailnet's SSH policy.

## Using the app

Health and inventory runner paths are read over SSH every minute, with at most four sessions at
a time. A failed or overdue check keeps the last successful values, marked stale; a metric that
can't be collected shows as unknown. Busy/idle and job details come from GitHub.

Each runner's version is compared with recent `actions/runner` releases, read hourly. The
runner pane and overview show runners that are behind. Needs attention lists a runner once the
oldest release it's missing is a week old, and calls the update overdue when the runner is more
than one minor release behind or has missed a release for over 30 days, since GitHub can then
stop sending it jobs. Jobs waiting for an approval or a concurrency group are counted on the
queue page, not listed.

**Machine diagnostics** on each machine pane covers Tailscale, SSH and startup, power settings,
SSH identity, disk capacity and runner services. It runs with the minute-by-minute check,
including on machines without runners, and current warnings appear in the fleet overview. macOS
checks AC sleep and power recovery; Linux checks systemd sleep masks and logind idle settings;
Windows checks AC and battery power plans and Tailscale unattended mode. The app never changes
these settings. **Runner checks** (**Run checks**) are the deeper, on-demand inventory and
prerequisite checks.

**Rename…** beside a machine's name, or **Edit…** in Settings ▸ Machines, gives it a display
name and description, saved in this Mac's app settings. The sidebar filter searches names,
descriptions, hosts, runners, labels and current jobs.

**Check now** on the overview (or **Check** on a machine) compares the fleet with the inventory.
Every change opens a review first; the [operating guide](docs/FLEET.md#using-the-app) covers
them.

To inspect one inventory host without the app (read-only JSON):

```sh
cargo run -q -p fleet --example fleet -- health mini
```

## Event history and alerts

The fleet overview and each machine show observed events, kept on this Mac while the app runs.
Desktop notifications are off by default: turn them on in Settings ▸ Notifications and use
**Test notification** to check delivery. The same tab adds alert destinations (Slack, Discord,
ntfy, JSON webhook) with **Send test**; their URLs and tokens are stored in the Keychain.
**Maintenance for 1 hour** on a machine pauses its notifications and destinations but keeps
history and in-app alerts. Thresholds and retention are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#event-history-and-alerts).

## Job history and Activity

Jobs are recorded when they finish on a self-hosted runner: the job scan follows each run it
saw queued or running, then reads how its jobs ended (conclusion, queue wait, run time, runner
and machine). Recording happens only while the app runs, and a job that starts and finishes
between two 45-second scans can be missed. **Activity** in the sidebar summarizes the last 24
hours, 7 days or 30 days; busy percentages use only the time this Mac was recording.

The **GitHub-hosted equivalent** is an estimate: each job's run time rounded up to whole minutes,
at per-minute rates for standard Linux, Windows and macOS runners (GitHub's published rates as
of 2026-10-26 by default). Edit the rates and their date in Settings ▸ Activity.

**Real-time job feed** (Settings ▸ Activity, off by default, organizations only) runs
`gh webhook forward --org=ORG --events=workflow_job` while the app is open. That creates a
temporary webhook on the organization and relays `workflow_job` events to a loopback receiver
on this Mac, which checks each delivery's signature. It needs the GitHub CLI extension
(`gh extension install cli/gh-webhook`) and the `admin:org_hook` scope
(`gh auth refresh --hostname github.com --scopes admin:org_hook`), as an organization owner.
GitHub allows one forwarder per organization at a time and describes forwarding as a development
tool; the app restarts it when it disconnects and keeps scanning every 3 minutes as a fallback.

## Local API for scripts and AI agents

While the app runs, it serves a read-only JSON API on `http://127.0.0.1:47622`:

```sh
curl -s localhost:47622/                       # capabilities, limits and semantics
curl -s localhost:47622/v1/summary             # counts, organization and tailnet status
curl -s 'localhost:47622/v1/machines?scope=all' # fleet and discovered machines
curl -s 'localhost:47622/v1/runners?activity=busy'
curl -s 'localhost:47622/v1/desired?machine=inv:studio'
curl -s 'localhost:47622/v1/events?limit=100'   # retained machine history
curl -s localhost:47622/v1/issues              # the desktop's attention findings
curl -s 'localhost:47622/v1/jobs/history?machine=inv:studio&limit=50' # completed jobs
curl -s 'localhost:47622/v1/analytics?window=7d' # busy time, waits and hosted-cost estimate
curl -s localhost:47622/v1/openapi.json        # OpenAPI 3.1 with generated view schemas
```

Port, discovery file, security, limits and every route: [docs/HTTP_API.md](docs/HTTP_API.md).

## Environment variables

| Variable | Effect |
| --- | --- |
| `RUNNER_DASHBOARD_INVENTORY` | Inventory path. Defaults to `inventory/fleet.toml` in the checkout the app was built from. |
| `RUNNER_DASHBOARD_GH` | Path to the `gh` binary, instead of searching for it. |
| `RUNNER_DASHBOARD_API_PORT` | Local API port (default 47622; `0` picks a free port). |

## Solo commands (`solo.yml`)

None of them start automatically.

- **Dev app**, **Build app** (`pnpm tauri build`, an ad-hoc signed `.app`) and **Browser
  preview**.
- **Install app to /Applications** runs `scripts/install-app.sh`. It builds the app and quits a
  running copy. The new copy is staged beside `/Applications/Runner Dashboard.app`, then
  swapped in with two renames.
- **Fleet parity** is the read-only comparison with Runner Fleet; see
  [docs/FLEET.md](docs/FLEET.md#parity-with-runner-fleet).
- **Tests** runs the verify loop from [AGENTS.md](AGENTS.md).

## More

- [docs/FLEET.md](docs/FLEET.md): operating the fleet and the inventory format.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): data flow, status rules and boundaries.
- [docs/HTTP_API.md](docs/HTTP_API.md): the local API.
- [AGENTS.md](AGENTS.md): rules for coding agents.
