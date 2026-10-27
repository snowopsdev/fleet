# Operating a runner fleet

`inventory/fleet.toml` is the local, Git-ignored source of truth for your self-hosted GitHub
Actions runners. It declares the organization, its trusted runner group, every host and runner.
The app compares it with GitHub and with the native service on each machine, shows the result
on each machine's and runner's pane, and makes changes only after you review them. The format,
rules and guards come from Runner Fleet's `bin/fleet` CLI.

The fleet uses long-lived native services: no Docker, Kubernetes, ARC, Ansible or VMs.
GitHub's runner auto-update stays on; **Upgrade…** catches up a runner that fell behind.

> Checking (status, plan, runner checks, logs) only reads. **Apply** never retires an existing
> runner. **Retire** needs an online, idle, active replacement and the phrase typed, and keeps
> the old directory. **Decommission** needs `decommission = true` in the inventory first, and
> also keeps the directory. **Clean up**, **Upgrade**, **Drain** and **Resume** never touch a
> busy runner. Every change opens a review; when you run it, the host is read again and nothing
> happens if it changed since the review. The only changes without a review each time are the
> two you turn on in Settings ([Automatic actions](#automatic-actions-opt-in)).

## Setup

The control Mac needs the tools in the README's [Requirements](../README.md#requirements).
Create your local inventory from the fictional example if it doesn't exist:

```sh
cp -n inventory/fleet.example.toml inventory/fleet.toml
```

Replace the example organization, SSH targets, service accounts and runners with your own
(see [Inventory format](#inventory-format)). Keep private addresses and host-specific operating
notes in `fleet.toml`; only the example is tracked. Override the path with
`RUNNER_DASHBOARD_INVENTORY`. Tests read the example and use fake GitHub and SSH boundaries, so
they don't need your private inventory.

Changes use the GitHub CLI login, never the token stored in Settings. The inventory manages
organization runners only: personal accounts and repository runners added in Settings ▸ GitHub
are monitored, never planned or applied. Listing organization runners needs the `admin:org`
scope:

```sh
gh auth refresh --hostname github.com --scopes admin:org
```

For a narrower long-term credential, give `gh` a fine-grained token
(`gh auth login --with-token`) with organization **Self-hosted runners: Read and write**. The
control Mac is the only machine with a durable GitHub credential; the runner machines never get
`gh` or a copy of it. Registration and removal tokens are minted at the moment of use, sent
only over SSH stdin, and never written to the inventory or to generated config.

Before the first apply, create the organization runner group named in your inventory
(**Acme trusted builders** in the example). Give it all private organization repositories and
all workflows in them, with public repositories disabled. That covers every current and future
private organization repository without allowlists. Apply checks this policy and refuses to
register into a looser group; the app never creates or loosens the group.

These are persistent machines: anyone who can change a workflow in an eligible private
repository can run code on them. Keep the runner group restricted to private repositories and
limit write access.

A runner group open to public repositories is worse: anyone can open a pull request from a
fork, and its workflow runs on your machines and can leave changes behind for later jobs. The
app warns (a GitHub finding) when the inventory's group allows public repositories, and, for
organizations added in Settings, when any runner group with self-hosted runners does. See
GitHub's [hardening for self-hosted runners](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions#hardening-for-self-hosted-runners).

## Inventory format

```toml
[organization]
name = "acme"
url = "https://github.com/acme"
runner_group = "Acme trusted builders"

[[hosts]]
id = "linux"
platform = "linux-x64"
ssh = "builder@linux.example.ts.net"
ssh_transport = "tailscale"
root = "~/gh-runners/acme"
service_mode = "systemd"
labels = ["fleet", "linux-build", "host-linux"]
runners = [{ name = "linux-01", directory = "runner-01" }]
```

`[organization]` names the organization, its URL (`https://github.com/NAME`) and the runner
group every runner registers into. Each `[[hosts]]` entry is one machine:

| Key | Meaning |
|---|---|
| `id` | Unique host ID. The local API accepts `inv:<id>` for the host's machine. |
| `platform` | `darwin-arm64`, `darwin-x64` (Intel Macs), `windows-arm64`, `windows-x64`, `linux-x64` or `linux-arm64` (Raspberry Pi, Ampere, Asahi). See [Platforms](#platforms). |
| `ssh` | SSH target, or `${NAME}` to read it from the environment variable `NAME`. |
| `ssh_transport` | `openssh` (default) or `tailscale`. |
| `root` | The dedicated directory that holds this host's runners. |
| `service_mode` | `launchagent` on macOS, `systemd` on Linux, `windows-service` or `windows-logon` on Windows. |
| `service_account` | Windows only, and required for `windows-logon`. |
| `labels` | Labels on every runner of the host: `fleet`, one platform capability label (`macos-build`, `windows-build`, `linux-build` or `linux-experimental`) and, by convention, `host-<id>`. |
| `enabled` | `false` leaves the host out of checks and changes. Defaults to `true`. |
| `xcode` | macOS only, optional: Xcode versions jobs expect, such as `["26.1"]`. See [Xcode and simulators](#xcode-and-simulators). |
| `simulator_runtimes` | macOS only, optional: simulator runtimes jobs expect, such as `["iOS 26.0"]`. |
| `mac_address`, `wake_via`, `wake_broadcast` | Optional; the first two together: the address to wake with Wake-on-LAN and the host on the same network that sends the packet. See [Wake a sleeping machine](#wake-a-sleeping-machine). |
| `runners` | At least one runner. |

A runner takes `name`, unique across the inventory, and `directory`, one path component below
`root`. Optional keys: `labels` (added to the host's), `exclude_labels` (host labels this runner
doesn't get; never `fleet`), `replaces = { name, directory }` for the legacy runner it
supersedes (add `repository = "org/repo"` for a repository-level one), and
`decommission = true`. Validation rejects duplicate names or paths, traversal, the wrong service
mode for the platform, and roots outside the runner area.

## Platforms

| `platform` | Runner release asset | `service_mode` | Capability label |
|---|---|---|---|
| `darwin-arm64` | `actions-runner-osx-arm64-*.tar.gz` | `launchagent` | `macos-build` |
| `darwin-x64` | `actions-runner-osx-x64-*.tar.gz` | `launchagent` | `macos-build` |
| `linux-x64` | `actions-runner-linux-x64-*.tar.gz` | `systemd` | `linux-build` or `linux-experimental` |
| `linux-arm64` | `actions-runner-linux-arm64-*.tar.gz` | `systemd` | `linux-build` or `linux-experimental` |
| `windows-x64` | `actions-runner-win-x64-*.zip` | `windows-service` or `windows-logon` | `windows-build` |
| `windows-arm64` | `actions-runner-win-arm64-*.zip` | `windows-service` or `windows-logon` | `windows-build` |

The architecture only selects the runner download. Install scripts, LaunchAgent and systemd
paths, NixOS modules and runner checks are the same for both architectures of an operating
system, and every download is verified against the SHA-256 digest GitHub publishes for that
asset. Machine diagnostics warn when a machine reports a different architecture than its
`platform`. `darwin-x64` and `linux-arm64` are not in Runner Fleet: an inventory that uses
them doesn't load there, so leave them out of inventories you compare with
[the parity script](#parity-with-runner-fleet).

## The example fleet

[`inventory/fleet.example.toml`](../inventory/fleet.example.toml) demonstrates macOS LaunchAgents,
Linux systemd, Windows services and Windows logon tasks. It includes a legacy migration, a
decommissioned runner, a GUI lane separated from build work, a Linux canary, an Intel Mac and
a Raspberry Pi.

Keep actual hardware, capacity and unattended-setup notes in your private inventory's comments.
Runners live in one dedicated root per host, for example:

```text
macOS/Linux: ~/gh-runners/acme/runner-01
Windows:     C:\gh-runners\acme\runner-01
```

To keep runner workspaces on another volume, such as an external SSD on a Mac mini, make
`~/gh-runners` a symlink to a directory on it. The machine's health then shows the runner
volume's free space, its diagnostics include a **Runner volume** check, and low-disk alerts
cover it like the system disk. If the volume isn't mounted, the symlink dangles and the check
warns **Not mounted** instead of reporting the home volume.

## Using the app

Machines come from the tailnet. Each inventory host is matched to its tailnet machine (as
`ssh -G` resolves its target) and listed with its runners: declared runners, legacy runners
still present, runners pending decommission, and GitHub runners carrying its `host-*` label.

- **Check now** (overview) or **Check** (a machine) reads GitHub with `gh` and probes hosts over
  SSH (`tailscale ssh` for `ssh_transport = "tailscale"`). The app checks once at launch, then
  on demand; health and local runner facts are also read over SSH every minute. An unreachable
  GitHub or host shows as *unknown*, never as missing, and one failure doesn't hide the other
  hosts.
- **Run checks** runs the runner checks (`doctor` in the CLI and the local API): SSH identity,
  prerequisites, disk, power policy and, for logon-task hosts, auto-logon and the active
  session. Results show on each machine.
- **Review apply…** (a machine's Changes section) installs or reconciles that host's runners:
  missing runners, labels, trusted-group membership, stopped services. On a host whose runners
  replace legacy ones, it applies one runner at a time.
- On a runner's pane: **Restart…** (only when it's idle), **Retire…**, **Decommission…**,
  **Upgrade…**, **Drain…** and **Resume…** ([Maintenance](#maintenance-cleanup-upgrades-and-drains)),
  and **Diagnostic log ▸ Show last 200 lines**, a read-only tail of its newest
  `_diag/Runner_*.log`.
- A machine's **Maintenance** section: **Clean up…** and **Upgrade idle runners…**.
- The Windows service-account password (`service_mode = "windows-service"` with
  `service_account`) and a Linux sudo password (for hosts without non-interactive sudo) are
  typed into the review when needed, used once, and never stored.

A new runner is installed like this:

1. Refuse GitHub-name collisions and non-empty, unexpected directories.
2. Fetch the current Actions runner release metadata from GitHub.
3. Download on the machine and verify the SHA-256 digest GitHub publishes.
4. Mint and send a short-lived registration token.
5. Register directly into the trusted group (`--runnergroup`), configure the directory and
   native service, and hold the service stopped.
6. Check and fix custom labels and group membership through the API.
7. Start the service only then, so an interrupted apply leaves a runner that can't take work.

Labels on an existing runner are changed through the labels API. A runner is never
re-registered and `--replace` is never used.

For a read-only JSON report without the app:
`cargo run -q -p fleet --example fleet -- status --json` (also `inventory`, `plan`, `doctor`,
and `health HOST`).

## Parity with Runner Fleet

`scripts/fleet-parity.sh` runs the read-only commands (`inventory`, `status`, `plan`, `doctor`)
through Runner Fleet's `bin/fleet` and through this port, and compares the JSON. It needs a
Runner Fleet checkout (`RUNNER_FLEET_DIR`, default `~/Code/soloterm/runner-fleet`) and python3.
It reads `RUNNER_DASHBOARD_INVENTORY` like the app. Free-disk numbers and runners outside the
inventory are ignored, since they move between the two runs. The `darwin-x64` and
`linux-arm64` platforms and the optional host keys `xcode`, `simulator_runtimes`,
`mac_address` and `wake_via` are additions of this app; Runner Fleet rejects them, so compare
inventories without them.

```sh
scripts/fleet-parity.sh              # all four commands
scripts/fleet-parity.sh plan doctor  # selected commands
```

## Workflow routing

Normal jobs route by the trusted group and a capability label, never a host:

```yaml
jobs:
  macos-build:
    runs-on:
      group: Acme trusted builders
      labels: macos-build
```

Use `windows-build` or `linux-build` for the other platforms, and add `xcode` when the job needs
Xcode. `host-*` labels are for diagnostics, not workload placement.

GUI E2E jobs select `macos-e2e`, `windows-e2e` or `windows-e2e-x64`. These are runner labels in
the inventory, never host labels. One eligible runner per lane serializes GUI jobs on that
desktop while builds use the other runners. The GUI slots use
`exclude_labels = ["macos-build"]` so builds can't occupy the desktop. Keep every label in the
inventory: apply removes custom labels the inventory doesn't declare.

Labels only govern GitHub scheduling. Manual GUI runs over SSH must use the desktop repository's
E2E harness and its singleton lock. A healthy runner service doesn't prove an interactive
desktop is ready.

## Native services by platform

| Platform | Service | Operating condition |
|---|---|---|
| macOS | per-user LaunchAgent | Jobs use the service owner's logged-in session (Xcode, keychain, GUI). Runners start after that user logs in following a reboot. |
| Linux | systemd | Starts before login and survives reboot. Install needs non-interactive sudo (or the sudo password in the review) for dependencies and the service. |
| Windows | service (`windows-service`) | Starts without login. Harden AC sleep and reboot policy, and make sure the service identity can reach every tool and certificate it needs. |
| Windows | logon task (`windows-logon`) | Runs `run.cmd` in `service_account`'s interactive desktop session, so GUI/WebView tests see a real desktop instead of Session 0. Needs auto-logon for that account; runner checks verify it. |

**Windows service identity.** The default runner service identity is `NETWORK SERVICE`, which
has no developer profile, certificates or per-user tool state. When jobs need a real build
user, set `service_account` (for example, `.\builder`) to keep its tool and profile state. The
password is asked for in the review. Runner checks can't prove a signing certificate is usable;
check that with a trusted test workflow.

**Interactive Windows runners** (`service_mode = "windows-logon"` with `service_account`).
Apply configures the runner without `--runasservice` and gives the account Modify on the runner
directory. It registers a Scheduled Task, `runner-fleet-NAME`, that starts `run.cmd` at that
account's logon with an interactive, non-elevated principal, under a headless console so no
window can be closed by accident. Task Scheduler only retries launch failures, so a second
trigger repeats every 5 minutes; it's skipped while the runner runs and only fires while the
account is logged on. The task stays disabled until group membership and labels are verified.
A `.fleet-task` marker in the runner directory lets status, restart, retire and decommission
find it; the runner counts as running when its own `Runner.Listener` is alive. No password is
sent. Set up auto-logon with Sysinternals Autologon before applying (it stores the credential
as an LSA secret). Keep that desktop dedicated to CI: signing another user in at the console
disconnects the runner's session, leaving it online without a live display.

**Modern Standby.** On an S0 laptop, turning the display off can enter standby even with sleep
set to never. Check AC sleep, display-off, lid actions and networking connectivity in standby
if the runner, Tailscale or SSH disconnects. A local policy at
`HKLM\SOFTWARE\Policies\Microsoft\Power\PowerSettings\f15576e8-98b7-4186-b944-eafa664402d9`
overrides the plan; its `ACSettingIndex` must be `1`, and a change takes effect after reboot.
Runner checks report each `power:*` setting. Nothing here ever changes power settings.

Runners that share a user account also share its caches and machine access; concurrent jobs
are not isolated. Record each host's capacity and operating constraints in the private inventory.

**macOS readiness.** Machine diagnostics on a Mac also read, without changing anything or
prompting: whether automatic login is set for the runner account
(`com.apple.loginwindow autoLoginUser`); which user is on the console and whether its screen
is locked (`IOConsoleUsers`, `CGSSessionScreenIsLocked`); whether the SSH session can read the
login keychain (`security show-keychain-info`, exit status only); and how many valid
code-signing identities there are (a count; identities never leave the machine). A locked
screen or another user on the console is a warning on hosts with a GUI lane (an `*-e2e`
label). SSH logins get their own security session, so a keychain that is locked for SSH may
still be unlocked for the LaunchAgent; the check says so instead of guessing. Every command has
a few seconds to finish, with stdin closed, and these checks share a 10-second budget: anything
that doesn't fit, such as a slow first `simctl` after boot, is reported as unknown rather than
delaying the machine's health and runner read.

### Xcode and simulators

Diagnostics list the Xcode apps in `/Applications` (read from each app's `version.plist`), the
selected developer directory (`xcode-select -p`) and whether the selected Xcode's license is
accepted (`xcodebuild -license check`). They warn when the Command Line Tools are selected
although Xcode is installed, and when the license isn't accepted. The `/usr/bin` shims are
never run, so a Mac without developer tools isn't offered an install dialog.

Declare the versions jobs rely on to get a warning when one is missing:

```toml
xcode = ["26.1", "16.4"]           # 26 accepts any 26.x; 26.1 accepts 26.1 and 26.1.x
simulator_runtimes = ["iOS 26.0"]  # read with simctl list runtimes, only when declared
```

Use simctl's runtime names: `iOS` (also for iPad simulators), `watchOS`, `tvOS` and
`visionOS`. The app never installs Xcode or runtimes. Simulator runtimes can't be listed until
the license is accepted.

**Tailscale SSH hosts** use `ssh_transport = "tailscale"` and explicit
`builder@HOST.example.ts.net` targets; scripts and transient passwords go over stdin. Tailscale
SSH must already be enabled and authorized on the machine. On macOS that means the open-source
`tailscaled`; the App Store and standalone apps don't serve Tailscale SSH.

**NixOS.** Before the first apply, copy
[`crates/fleet/scripts/nixos-runner-prerequisites.nix`](../crates/fleet/scripts/nixos-runner-prerequisites.nix)
to `/etc/nixos/runner-fleet-prerequisites.nix`, import it, and run
`sudo nixos-rebuild switch`. It supplies `nix-ld` libraries and `/bin/bash` for the runner
and its auto-update scripts. Apply then generates a per-runner module,
`/etc/nixos/runner-fleet-NAME.nix`, imports it while keeping a backup of
`configuration.nix`, and rebuilds; the service stays closed until labels and group membership
are verified. Don't also enable `services.github-runners` for inventory runners.

## Add a host

1. Make sure it's on the tailnet and reachable over SSH from this Mac: Remote Login on macOS,
   OpenSSH Server on Windows, or Tailscale SSH. Its host key should already be trusted; the app
   never adds host keys.
2. Add it to `inventory/fleet.toml` (see [Inventory format](#inventory-format)):

   ```toml
   [[hosts]]
   id = "ubuntu-mini-01"
   platform = "linux-x64"
   ssh = "builder@ubuntu-mini-01.example.ts.net"
   ssh_transport = "tailscale"
   root = "~/gh-runners/acme"
   service_mode = "systemd"
   labels = ["fleet", "linux-build", "host-ubuntu-mini-01"]
   runners = [{ name = "ubuntu-mini-01-01", directory = "runner-01" }]
   ```

3. In the app: **Check now**, then **Run checks** for the new host, then **Review apply…**.

## Wake a sleeping machine

A machine that sleeps or powers off drops off the tailnet, so nothing can reach it directly.
If its network card supports Wake-on-LAN, declare its address and another inventory host on
the same network:

```toml
mac_address = "02:00:5e:10:00:01"  # six hex pairs, ':' or '-'
wake_via = "studio"                # another [[hosts]] id
wake_broadcast = "192.168.1.255"   # optional; defaults to 255.255.255.255
```

Set `wake_broadcast` to the machine's subnet broadcast address when the sending host is on
several networks, so the packet leaves on the right one.

When the machine is offline, its pane offers **Wake…**. The review shows the address, the
sending host and the broadcast address (`255.255.255.255:9` unless set); it's blocked unless the
sending host is online on the tailnet and the machine isn't. Sending re-reads the inventory, the
tailnet and the sending host's SSH configuration and refuses if any changed, then runs a small script on the sending host over SSH: python3, or perl, broadcasts
one magic packet (a bash `/dev/udp` redirect can't broadcast); Windows hosts use .NET's
`UdpClient`. Only the validated address reaches the script, as twelve hex digits. The machine's
history records the wake. Enable Wake-on-LAN on the machine itself first (on macOS, **Wake for
network access**, reported by diagnostics); a packet doesn't prove the machine woke.

## Replace and retire a runner

Never move or rename a configured runner directory: LaunchAgent plists hold absolute paths.
Declare the new runner with `replaces = { name = "...", directory = "..." }`, install it beside
the old one, check it, and only then retire the old one. Capacity never drops.

1. **Review apply…** for the new runner only. The old one is untouched.
2. **Check now**: both online, both services active. Let it take a real CI job before
   retiring anything.
3. The legacy runner's pane now offers **Retire…** (the plan says *retire-legacy*, not
   *preserve-legacy*). Type the phrase it asks for. The old directory is kept for recovery.

Stop if GitHub or the host can't be read, the new service is inactive, the replacement is
offline or busy, or the old runner is busy. Runner workspaces are disposable: `_work` isn't
copied.

## Decommission a runner

Remove capacity without a replacement by declaring it first, so no later apply reinstalls it:

```toml
runners = [{ name = "linux-01", directory = "runner-01", decommission = true }]
```

The runner then shows *decommission pending*, and its pane offers **Decommission…**. It refuses
a busy runner. It also refuses, for manual inspection, a GitHub entry whose directory isn't
configured and a configured directory with no GitHub entry. Otherwise it stops and uninstalls
that exact service and deregisters with a short-lived removal token; on NixOS it also removes
the generated module, keeping a `configuration.nix.before-decommission-NAME` backup. An offline
registration with no directory is deleted through the API. It always waits until GitHub stops
listing the name and keeps the runner directory. The entry stays in the inventory as a
tombstone that reserves the name and path; remove it, and the directory, later and
deliberately.

## Maintenance: cleanup, upgrades and drains

These are reviewed changes like apply: the review re-reads GitHub and the host, and running it
reads both again and refuses if anything changed. A busy runner is never cleaned, upgraded or
stopped, and the host scripts check once more, immediately before acting, that no
`Runner.Worker` from that runner's directory is running.

### Clean up

**Clean up…** on a machine measures, then (after review) removes, only these targets. Each is
shown with its exact path and measured size:

| Target | What is removed |
|---|---|
| `RUNNER/_work/_temp`, `RUNNER/_work/_actions` | Their contents, for idle runners. |
| `RUNNER/_work` | Its whole contents (workspaces, tool cache, temp), only with the review's opt-in checkbox. |
| `RUNNER/_diag` | `Runner_*.log` and `Worker_*.log` files directly in it, older than the chosen number of days (14 by default). The review lists each file; only those are removed. |
| `RUNNER/bin.VERSION`, `RUNNER/externals.VERSION` | Previous runner versions that `bin` and `externals` no longer point at, kept by an upgrade or the runner's own self-update. Each folder is listed by name; the version in use never is. Kept while the runner is held after a failed change, while an upgrade's swap is unfinished, and whenever `bin` or `externals` is missing or doesn't resolve to a `NAME.VERSION` folder in the runner directory (then the version in use can't be told apart). |
| `RUNNER/.fleet-upgrade.*` | Staging folders an interrupted upgrade left behind, by name, only while no upgrade or cleanup holds the runner's lock and no swap is unfinished. |
| `~/Library/Developer/Xcode/DerivedData` (macOS) | Its contents, only when no runner on the machine is busy. |
| Unavailable simulators (macOS) | `xcrun simctl delete unavailable`, only when no runner is busy. |
| Docker (macOS, Linux) | `docker system prune -f` when Docker is installed and running: stopped containers, unused networks, dangling images and build cache. Never volumes. Only when no runner is busy. |

`RUNNER` is a declared runner's exact inventory directory. The host derives every path from a
fixed suffix of it (or of the home directory), requires `.runner` to name that runner, resolves
the path, and refuses one that is a symlink, passes through one, or resolves anywhere else. It
removes directory contents with `find -delete`, which neither follows links nor crosses into
another file system, and keeps the directory itself. On Windows it uses PowerShell and .NET,
removing junctions without following them. `.runner`, `.credentials*`, configuration files and
the `bin` and `externals` in use are never targets. Legacy and decommissioned runner directories
are left alone.

Each runner's service is stopped before its directories are cleaned, with the same job check as
**Drain**, so it can't pick up a job while files are removed, and started again afterwards if it
was running (a drained runner is never stopped or started by a cleanup, and a draining one keeps
its directories). Machine-wide targets (DerivedData, simulators, Docker) are cleaned only when
every runner on the machine (legacy and decommission-pending ones too) is definitely stopped or
is a declared runner this cleanup stops; an unknown state counts as running, and the host
refuses while any `Runner.Listener` or `Runner.Worker` runs there. The host refuses a runner's
target while that runner's service, a job, or anything else started from its directory runs, and
while a runner self-update is in progress: the runner writes `_work/_update.sh` (`.cmd` on
Windows) and then `update.finished` when it's done, so a script written in the last 30 minutes
without a newer `update.finished` counts as in progress. (`_work/_update` itself stays after
every self-update and doesn't block anything.) A runner's processes are matched by its `bin` and
`bin.VERSION` paths; on Windows they're attributed through CIM (`Win32_Process` executable path
or command line), a runner process whose path can't be read counts as running, and an elevated
SSH session attributes them precisely.

While it removes files in a runner directory, the host script holds `RUNNER/.fleet-maintenance`
with the process groups doing the work; an upgrade holds the same lock while it runs. Starting,
restarting or upgrading that runner (including the watchdog's restarts) refuses while any of
them is alive. A lock whose processes are gone is stale and removed; one whose PID has since been
reused by an unrelated process stays live, so a recycled PID fails closed (remove the lock by
hand once you've checked nothing is working there). The script ignores a dropped connection and
has a 14-minute deadline, before the app gives up waiting: each removal, size measurement and
Docker prune runs in its own process group, and at the deadline the whole group is stopped and
waited for, so nothing keeps deleting after the lock is released. A removal stopped part-way
that way, or a lost connection, means the app does not start the runners it stopped: they stay
stopped and held, since their directories may be half-emptied. If a runner can't be started
again, the cleanup reports it and holds it stopped (see [Recovery](#recovery)).

This is a deliberate, narrow exception to Runner Fleet's "no broad or glob-based deletion"
rule: these runner caches grow without bound, and each target is an enumerated, reviewed,
exact path. Nothing else is ever removed recursively.

**Cleaning after every job (manual recipe).** The runner can run a script after each job with
`ACTIONS_RUNNER_HOOK_JOB_COMPLETED`. The app doesn't install one; if you want it, add to the
runner directory's `.env` (one runner at a time, with its service stopped, then start it):

```sh
ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/Users/builder/gh-runners/acme/job-completed.sh
```

and keep the script narrow, for example:

```sh
#!/bin/sh
# Runs after each job, in the runner's own account. Remove only this job's temp files.
set -eu
[ -n "${RUNNER_TEMP:-}" ] && [ -d "$RUNNER_TEMP" ] || exit 0
find "$RUNNER_TEMP" -xdev -mindepth 1 -delete
```

A failing hook fails the job, so test it on one runner first. `.env` survives upgrades, since
upgrades only switch `bin` and `externals`.

### Upgrade

For a runner behind the latest `actions/runner` release, **Upgrade…** (a runner's pane) or
**Upgrade idle runners…** (a machine's Maintenance section) shows each runner's current and
target version; busy runners appear as not now. Running it handles one runner at a time: stop
the idle service, download the release on the machine and verify the SHA-256 digest GitHub
publishes (a release without one is refused), and install it the way the runner's own
self-update does: the release goes to `bin.VERSION` and `externals.VERSION` beside the current
ones, and `bin` and `externals` become links to them (on Windows, junctions). A runner still on
plain `bin/` and `externals/` folders is converted the same way its self-update would convert
it, renaming them to `bin.OLDVERSION` and `externals.OLDVERSION`. The old version stays beside
the new one as the rollback copy until you remove it with **Clean up…**, or until the runner's
own next self-update: that keeps only the running and the new version's folders and prunes the
rest, including this copy. Like a self-update, it then refreshes the root files the package
carries (`run.sh`, `config.sh`, `env.sh`, `safe_sleep.sh`, the `run-helper` templates, and on
Windows `run.cmd` and `config.cmd`) and the root `runsvc.sh` from the new `bin`, each through a
temporary file and a rename. Then it starts the service and confirms the runner reports the new
version; one that reports another version is stopped again and held. The directory is never
moved and `.runner`, `.credentials*`, `.env` and other dotfiles are never touched.

Before switching the links it checks again that nothing runs from the runner directory, writes
`RUNNER/.fleet-upgrade-swap` naming the folders that hold the old version, and removes it once
both links point at the new version: that removal is the moment the upgrade completes. A failure
while switching points the links back at once; a swap interrupted by a dropped connection is
rolled back by the next upgrade before anything else, so `bin` and `externals` never end up on
different versions and the old version is never removed. The marker is removed only when every
link is back; if one can't be, the upgrade fails and says how to finish by hand. While the
marker exists, starting or restarting the runner refuses. The host refuses an upgrade while
anything runs from the runner directory, while its self-update is in progress (judged, as for
cleanup, by `_work/_update.sh` written in the last 30 minutes without a newer `update.finished`,
so a stalled self-update blocks for at most 30 minutes), while a cleanup or another upgrade
holds its lock, and when `bin` or `externals` links outside the runner directory.

A runner that started a job in the meantime is skipped; a failure stops there and leaves that
runner stopped and *held*: the watchdog leaves it alone until a reviewed **Resume…**,
**Restart…** or upgrade (see [Recovery](#recovery)). Every runner an upgrade or cleanup may stop
is held durably before anything is stopped, and released when it finishes, so a crash or quit
mid-change leaves them held rather than silently stopped. If a newer release appears between
review and run, the run refuses and you review again. A drained runner can't be upgraded until
it's resumed or its drain is cancelled.

### Drain and resume

GitHub has no API to pause a runner, so **Drain…** works at the service level: the runner's
service is stopped on the machine once it is idle, immediately if it already is. A busy runner
keeps its job: the app keeps a durable drain (in `automation.json` beside its settings) and,
about every 20 seconds, stops the service once GitHub shows the runner idle. That stop is
authorised by the original review: it checks that the inventory still declares the same runner
at the same path and that GitHub lists the same registration, refuses a busy runner, and is
recorded in the machine's history. **Cancel drain** on the runner's pane removes a drain that
hasn't stopped anything yet. A drain that hasn't found the runner idle within 24 hours is
cancelled, with an event and a notification: drain again if you still want it. While a drain
holds a runner, it can't be drained again, restarted, upgraded, or started by **Review apply…**.
The sidebar marks drained runners *draining* or *paused*.

A drained runner shows offline on GitHub and the watchdog leaves it alone. **Resume…** starts its
service and ends the drain. The stop lasts until the service manager starts it again: a
LaunchAgent after the next login, a systemd unit or Windows service after a reboot. If the app
then sees it running, it ends the drain and notes that in history. A logon-task runner's task is
disabled while drained, so its repeating trigger can't restart it.

A deferred stop runs without a password. On a Linux host without non-interactive sudo it can't
stop the service; the drain stays and its pane shows why. Drain it again once it's idle, with the
sudo password in the review.

## Automatic actions (opt-in)

Two settings let the app change things without a review each time. Both are off by default, and
turning one on is the standing approval for exactly what it does. This is a deliberate, narrow
exception to "only reviewed plans change machines". Neither runs with demo data.

### Watchdog restarts

**Settings ▸ General ▸ Automatically restart stopped runner services.** When on, the app
restarts an inventory runner (not a legacy, decommissioned or drained one) on an enabled host
when a fresh SSH read shows its service `inactive`, `failed` or `stopped` and fresh GitHub data
shows it offline and not busy. It uses the same guarded restart as **Restart…**, which reads
GitHub again and refuses a busy runner. It waits 2 minutes after first seeing the service
stopped, then 5, 15 and 60 minutes between attempts, and makes at most 3 attempts per runner in
6 hours. When a fourth would be due it gives up, raises an alert and stops trying until it sees
the service running again. Every attempt, failure, give-up and recovery is recorded in the
machine's history and sent through desktop notifications and alert destinations (maintenance
mode pauses those). It never restarts a runner a failed upgrade or cleanup left held stopped.
Attempts are saved before they are made, so restarting the app doesn't
repeat one. Turning the setting off or on starts its history afresh. It never uses a password,
so hosts that need a sudo password can't be restarted this way.

### Fleet availability for workflows

**Settings ▸ General ▸ Publish availability to an organization variable.** When on, the app
writes the organization Actions variable `RUNNER_FLEET_STATUS` (the name is configurable),
visible to the organization's private repositories, through `gh api` with the `gh` login (which
needs organization admin, as for other changes):

```json
{"updatedAt":"2026-10-03T12:00:00Z","online":{"linux-build":1,"macos-build":2},"healthy":true}
```

`online` counts the inventory runners GitHub reports online for each label the inventory gives
them, except `fleet` and `host-*`; `healthy` means every one of those labels has at least one.
Online runners can still be busy. It is written only from fresh GitHub data (the organization
must also be added in Settings ▸ GitHub), every 5 minutes (configurable) whether or not the
counts changed, so `updatedAt` always says how current it is. Nothing else is written. While the
app is closed or the Mac sleeps, or GitHub data is stale, nothing is written and `updatedAt`
grows old, so readers must treat an old value as unknown.

Workflows can then fall back to GitHub-hosted runners when self-hosted capacity is down
([community discussion #20019](https://github.com/orgs/community/discussions/20019)). Use a
small routing job that checks freshness first; here anything older than 20 minutes (four
publishing intervals) counts as down:

```yaml
jobs:
  route:
    runs-on: ubuntu-latest
    outputs:
      runner: ${{ steps.pick.outputs.runner }}
    steps:
      - id: pick
        env:
          STATUS: ${{ vars.RUNNER_FLEET_STATUS }}
        run: |
          runner='"macos-latest"'
          if [ -n "$STATUS" ]; then
            updated=$(jq -r '.updatedAt // empty' <<<"$STATUS")
            online=$(jq -r '.online["macos-build"] // 0' <<<"$STATUS")
            if [ -n "$updated" ]; then
              age=$(( $(date -u +%s) - $(date -u -d "$updated" +%s) ))
              if [ "$age" -lt 1200 ] && [ "$online" -gt 0 ]; then
                runner='{"group":"Acme trusted builders","labels":"macos-build"}'
              fi
            fi
          fi
          echo "runner=$runner" >> "$GITHUB_OUTPUT"
  build:
    needs: route
    runs-on: ${{ fromJSON(needs.route.outputs.runner) }}
```

The routing job's output is JSON, so `fromJSON` yields either the trusted-group object or the
string `macos-latest`. A shortcut without the routing job reads the variable directly, but it
**can't check `updatedAt`**, so a stale `online` count (the app closed while runners went down)
keeps routing to self-hosted runners:

```yaml
    runs-on: ${{ vars.RUNNER_FLEET_STATUS && fromJSON(vars.RUNNER_FLEET_STATUS).online['macos-build'] > 0 && fromJSON('["self-hosted","macos-build"]') || 'macos-latest' }}
```

`vars` is available in `runs-on`; the leading `vars.RUNNER_FLEET_STATUS &&` keeps `fromJSON`
from failing before the variable exists, and a missing label reads as null, which isn't greater
than 0. Expressions bind `>` before `&&` before `||`. Jobs that need the machines themselves
(GUI lanes, signing) shouldn't fall back.

## Recovery

- **Install failed before registration:** fix the reported prerequisite. A partial, unconfigured
  extraction in the new directory needs manual inspection; nothing overwrites unexpected data.
- **Registered, but the group or labels failed:** don't retry blindly. The plan shows the
  drift, and **Review apply…** fixes it through the API.
- **Service down, runner idle:** **Restart…**, then **Check now** and **Diagnostic log ▸ Show
  last 200 lines**.
- **Retire or decommission interrupted:** **Check now** and inspect both registrations. If the
  service is gone but GitHub still lists the runner, decommission again. Never re-register a
  decommissioned name without first removing its tombstone.
- **Restoring a retired runner:** its files are still at the legacy path, but its registration
  is gone. That's a manual procedure, not an automatic rollback.

- **Upgrade failed:** the runner is left stopped and held, with `bin` and `externals` pointing
  at the old version (a failure while switching points them back by itself; an interrupted
  switch is rolled back by the next upgrade). Read **Diagnostic log**, then **Resume…** or
  **Restart…**. While `.fleet-upgrade-swap` exists, the runner won't start: run the upgrade
  again, or by hand, with the service stopped, `ln -sfn RUNNER/bin.OLDVERSION RUNNER/bin`, the
  same for `externals`, and remove `.fleet-upgrade-swap`.
- **Held stopped:** a failed upgrade or cleanup left the runner stopped on purpose, or one was
  interrupted (the app quit or crashed); its pane says why. Fix the cause, then **Resume…** (or
  **Restart…** or upgrade again), which releases it. **Review apply…** won't start a held runner.
  A held runner seen running again (started by hand or after a reboot) is released by itself,
  with an event.
- **A cleanup lost contact:** its runners stay stopped and held. Check the machine, wait for any
  `RUNNER/.fleet-maintenance` to disappear (or its processes to end), then **Resume…**.
- **The watchdog gave up:** an alert says so. Read the diagnostic log, fix the cause, and
  **Restart…** the runner; the watchdog tries again once it has seen the service running.

The only recursive removal is **Clean up…**, and only for the exact targets it lists. Remove old
runner directories by explicit path, separately.

## Troubleshooting

| Symptom | Check |
|---|---|
| GitHub columns are *unknown* | `gh auth status --hostname github.com`; refresh `admin:org`, or re-run `gh auth login` with a suitable token. |
| "runner group 'NAME' does not exist; create and restrict it in GitHub first" | Create the group named in the inventory and restrict it. The name is case-sensitive. |
| Host can't be read | Check whether it is awake and reachable over Tailscale and SSH. |
| macOS runners offline after a reboot | Log in as the LaunchAgent owner. |
| Linux install fails on sudo | Use the sudo password field in the review, or configure narrowly scoped non-interactive sudo. |
| Windows job can't see a certificate or profile | Check the runner's service identity; use a dedicated build account only when needed. |
| Logon-task runner offline | **Run checks**: auto-logon must be set and the account's session active. |
| GUI E2E fails with focus or visibility errors on a Mac | A locked desktop fails every GUI test: the machine's **Screen lock** diagnostic reads `IOConsoleUsers`' `CGSSessionScreenIsLocked`. |
| `xcodebuild` fails on a Mac with Xcode installed | Check the **Selected developer directory** and **Xcode license** diagnostics. |
| "GitHub name exists but exact local directory is missing" | Resolve the collision by hand. Apply never uses `--replace`. |
| Local `.runner` exists but GitHub has no runner | Inspect before re-registering. Apply won't tear it down. |
