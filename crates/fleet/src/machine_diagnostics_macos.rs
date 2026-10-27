//! macOS CI readiness: automatic login, the console session and screen lock, the login
//! keychain, code signing, and installed Xcode versions compared with the inventory.

use super::Checks;
use super::DiagnosticGroup::{Session, Toolchain};
use super::DiagnosticStatus::{Info, Pass, Unknown, Warn};
use crate::desired::model::{Host, ServiceMode};

/// `xcodebuild -license check` exits 69 (`EX_UNAVAILABLE`) until the license is accepted.
const LICENSE_NOT_ACCEPTED: &str = "69";
/// `security` exits 36 when the keychain is locked and unlocking would need interaction.
const KEYCHAIN_INTERACTION_NOT_ALLOWED: &str = "36";
/// A bounded command killed at its time limit (`SIGKILL`).
const TIMED_OUT: &str = "137";
/// A check skipped because the readiness block's overall deadline had passed.
const SKIPPED: &str = "124";
/// `defaults read` exits 1 when the key doesn't exist: automatic login is off.
const DEFAULTS_KEY_MISSING: &str = "1";

/// Why a bounded check has no answer, for its detail text.
fn unanswered(code: &str, what: &str) -> String {
    match code {
        TIMED_OUT => format!("{what} didn't finish in time."),
        SKIPPED => format!("{what} was skipped: the readiness checks ran out of time."),
        code => format!("{what} exited {code}."),
    }
}

/// An installed Xcode: version, build and app path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledXcode<'a> {
    pub version: &'a str,
    pub build: &'a str,
    pub path: &'a str,
}

/// Parses the `version|build|path;…` fact.
pub(crate) fn installed_xcodes(fact: &str) -> Vec<InstalledXcode<'_>> {
    fact.split(';')
        .filter_map(|entry| {
            let mut parts = entry.splitn(3, '|');
            let (version, build, path) = (parts.next()?, parts.next()?, parts.next()?);
            (!version.is_empty()).then_some(InstalledXcode { version, build, path })
        })
        .collect()
}

/// Whether an installed version satisfies a declared one: `26` accepts any 26.x, `26.1`
/// accepts 26.1 and 26.1.x, and missing trailing components count as zero (`26` is `26.0`).
pub(crate) fn version_satisfies(declared: &str, installed: &str) -> bool {
    let installed: Vec<&str> = installed.trim().split('.').collect();
    declared.trim().split('.').enumerate().all(|(i, part)| {
        let have = installed.get(i).copied().unwrap_or("0");
        have.parse::<u64>()
            .ok()
            .zip(part.parse::<u64>().ok())
            .is_some_and(|(a, b)| a == b)
    })
}

/// `iOS 26.0` against installed runtime names, with the same version rule.
pub(crate) fn runtime_satisfies(declared: &str, installed: &str) -> bool {
    match (declared.split_once(' '), installed.trim().split_once(' ')) {
        (Some((os, version)), Some((have_os, have_version))) => {
            let platform = |os: &str| {
                if os.eq_ignore_ascii_case("xrOS") {
                    "visionos".to_string()
                } else {
                    os.to_ascii_lowercase()
                }
            };
            platform(os) == platform(have_os) && version_satisfies(version, have_version)
        }
        _ => false,
    }
}

impl Checks<'_> {
    /// Whether the inventory runs this machine's runners as LaunchAgents.
    fn launchagent(host: Option<&Host>) -> bool {
        host.is_some_and(|h| h.service_mode == ServiceMode::Launchagent && h.desired_runners().next().is_some())
    }

    /// A runner takes GUI jobs (an `*-e2e` lane, by the inventory's convention).
    fn gui_lane(host: Option<&Host>) -> bool {
        host.is_some_and(|h| {
            h.desired_runners().any(|r| {
                h.labels_for(r)
                    .iter()
                    .any(|l| l.ends_with("-e2e") || l.contains("-e2e-"))
            })
        })
    }

    pub(super) fn macos_session(&mut self, host: Option<&Host>) {
        let account = self.get("identity").unwrap_or("the runner account");
        let runners = Self::launchagent(host);
        let attention = if runners { Warn } else { Info };

        let (status, value, detail) = match (self.get("auto_login_status"), self.get("auto_login_user")) {
            (Some("0"), Some(user)) if user == account => (
                Pass,
                format!("Enabled for {user}"),
                "After a restart, macOS logs in as the runner account, so its LaunchAgents start.".to_string(),
            ),
            (Some("0"), Some(user)) => (
                attention,
                format!("Logs in as {user}"),
                format!("After a restart, macOS logs in as {user}; LaunchAgent runners wait until {account} logs in."),
            ),
            (Some(DEFAULTS_KEY_MISSING), _) => (
                Info,
                "Off".into(),
                format!(
                    "After a restart, LaunchAgent runners wait until someone logs in as {account}. FileVault turns automatic login off."
                ),
            ),
            (Some(code), _) if code != "0" => (
                Unknown,
                "Unknown".into(),
                unanswered(code, "Reading the login window preferences"),
            ),
            _ => (
                Unknown,
                "Unknown".into(),
                "The login window preferences could not be read.".into(),
            ),
        };
        self.add("auto-login", Session, "Automatic login", status, &value, detail);

        let user = self.get("console_session_user");
        let (status, value, detail) = match (self.get("console_sessions"), user) {
            (None, _) => (
                Unknown,
                "Unknown".to_string(),
                "The console sessions (IOConsoleUsers) could not be read.".to_string(),
            ),
            (Some(_), Some(user)) if user == account => (
                Pass,
                user.to_string(),
                "The runner account is the console session, so GUI jobs have a display.".into(),
            ),
            (Some(_), Some(user)) => (
                if Self::gui_lane(host) { Warn } else { Info },
                user.to_string(),
                format!(
                    "{user} is on the console, not {account}. GUI tests from {account}'s runners need its session on the console."
                ),
            ),
            (Some(_), None) => (
                attention,
                "Nobody logged in".into(),
                format!("LaunchAgent runners start only after {account} logs in."),
            ),
        };
        self.add("console-session", Session, "Console session", status, &value, detail);

        if user.is_some() {
            let (status, value) = match self.get("screen_locked") {
                Some("yes") => (if Self::gui_lane(host) { Warn } else { Info }, "Locked"),
                Some("no") => (Pass, "Unlocked"),
                _ => (Unknown, "Unknown"),
            };
            self.add(
                "screen-lock",
                Session,
                "Screen lock",
                status,
                value,
                "From CGSSessionScreenIsLocked. GUI tests, such as UI tests in the simulator, fail on a locked desktop.".into(),
            );
        }

        let (status, value, detail) = match self.get("login_keychain") {
            Some("0") => (
                Pass,
                "Unlocked".to_string(),
                "This SSH session can read the login keychain without a prompt.".to_string(),
            ),
            Some(KEYCHAIN_INTERACTION_NOT_ALLOWED) => (
                Info,
                "Locked for SSH sessions".into(),
                "SSH logins get their own security session, so this can't show whether the runner's LaunchAgent session has the keychain unlocked. Signing jobs need it unlocked there.".into(),
            ),
            Some("missing") => (
                attention,
                "Not found".into(),
                format!("{account} has no login keychain, so code signing has no identities to use."),
            ),
            Some(code) => (
                Unknown,
                "Unknown".into(),
                unanswered(code, "security show-keychain-info"),
            ),
            None => (Unknown, "Unknown".into(), "The login keychain status could not be read.".into()),
        };
        self.add("login-keychain", Session, "Login keychain", status, &value, detail);

        let identities = self.get("codesign_identities").and_then(|v| v.parse::<u32>().ok());
        self.add(
            "codesign-identities",
            Session,
            "Code-signing identities",
            if identities.is_some() { Info } else { Unknown },
            &identities.map_or_else(|| "Unknown".into(), |n| format!("{n} valid")),
            "Valid code-signing identities in this account's keychains, counted without reading them.".into(),
        );
    }

    pub(super) fn xcode(&mut self, host: Option<&Host>) {
        let scanned = self.get("xcode_scan").is_some();
        let installed = self.get("xcode_apps").map(installed_xcodes).unwrap_or_default();
        let developer = self.get("developer_dir");
        let declared = host.map(|h| h.xcode.as_slice()).unwrap_or_default();

        let listed = installed
            .iter()
            .map(|x| {
                if x.build.is_empty() {
                    x.version.to_string()
                } else {
                    format!("{} ({})", x.version, x.build)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.add(
            "xcode-installed",
            Toolchain,
            "Installed Xcode",
            if scanned { Info } else { Unknown },
            if !scanned {
                "Unknown"
            } else if installed.is_empty() {
                "None"
            } else {
                &listed
            },
            if !scanned {
                "Installed Xcode versions could not be read.".into()
            } else if installed.is_empty() {
                "No Xcode app in /Applications. Read from each app's version.plist; nothing is installed.".into()
            } else {
                installed
                    .iter()
                    .map(|x| format!("{}: {}", x.version, x.path))
                    .collect::<Vec<_>>()
                    .join("; ")
            },
        );
        if !scanned {
            return;
        }

        let selected = developer.and_then(|d| installed.iter().find(|x| d.starts_with(x.path)));
        let (status, value, detail) = match (developer, selected) {
            (None, _) => (
                if installed.is_empty() { Info } else { Unknown },
                "Not set".to_string(),
                "xcode-select reports no developer directory.".to_string(),
            ),
            (Some(dir), Some(xcode)) => (Pass, format!("Xcode {}", xcode.version), dir.to_string()),
            (Some(dir), None) if dir.contains("CommandLineTools") && !installed.is_empty() => (
                Warn,
                "Command Line Tools".into(),
                format!(
                    "{dir} is selected, so xcodebuild and simulators aren't available to jobs. An administrator can run sudo xcode-select -s {}.",
                    installed[0].path
                ),
            ),
            (Some(dir), None) => (Info, dir.to_string(), "The selected developer directory.".into()),
        };
        self.add(
            "xcode-selected",
            Toolchain,
            "Selected developer directory",
            status,
            &value,
            detail,
        );

        if !installed.is_empty() {
            let dir = self.get("xcode_license_dir").unwrap_or("Xcode");
            let (status, value, detail) = match self.get("xcode_license") {
                Some("0") => (Pass, "Accepted", format!("xcodebuild -license check passed for {dir}.")),
                Some(LICENSE_NOT_ACCEPTED) => (
                    Warn,
                    "Not accepted",
                    format!(
                        "xcodebuild refuses to build until an administrator runs sudo xcodebuild -license accept ({dir})."
                    ),
                ),
                Some(code) => (Unknown, "Unknown", unanswered(code, "xcodebuild -license check")),
                None => (Unknown, "Unknown", "The license check couldn't run.".into()),
            };
            self.add("xcode-license", Toolchain, "Xcode license", status, value, detail);
        }

        if !declared.is_empty() {
            let missing: Vec<&str> = declared
                .iter()
                .filter(|d| !installed.iter().any(|x| version_satisfies(d, x.version)))
                .map(String::as_str)
                .collect();
            let have = if installed.is_empty() {
                "none".to_string()
            } else {
                listed.clone()
            };
            self.add(
                "xcode-declared",
                Toolchain,
                "Declared Xcode",
                if missing.is_empty() { Pass } else { Warn },
                &if missing.is_empty() {
                    "All installed".to_string()
                } else {
                    format!("Missing {}", missing.join(", "))
                },
                format!(
                    "The inventory declares Xcode {}; installed: {have}.{}",
                    declared.join(", "),
                    if missing.is_empty() {
                        ""
                    } else {
                        " Jobs that select a missing version fail. The app never installs Xcode."
                    }
                ),
            );
        }

        let runtimes = host.map(|h| h.simulator_runtimes.as_slice()).unwrap_or_default();
        if !runtimes.is_empty() {
            let available: Vec<&str> = match (self.get("simulator_scan"), self.get("simulator_runtimes")) {
                (Some(_), Some(list)) => list.split(';').collect(),
                _ => Vec::new(),
            };
            let (status, value, detail) = if self.get("simulator_scan").is_none() {
                (
                    Unknown,
                    "Unknown".to_string(),
                    if self.get("xcode_license") == Some(LICENSE_NOT_ACCEPTED) {
                        "Simulator runtimes can't be listed until the Xcode license is accepted.".to_string()
                    } else {
                        "simctl list runtimes couldn't be read with an installed Xcode.".to_string()
                    },
                )
            } else {
                let missing: Vec<&str> = runtimes
                    .iter()
                    .filter(|d| !available.iter().any(|a| runtime_satisfies(d, a)))
                    .map(String::as_str)
                    .collect();
                let have = if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                };
                (
                    if missing.is_empty() { Pass } else { Warn },
                    if missing.is_empty() {
                        "All installed".to_string()
                    } else {
                        format!("Missing {}", missing.join(", "))
                    },
                    format!("The inventory declares {}; available: {have}.", runtimes.join(", ")),
                )
            };
            self.add(
                "simulator-runtimes",
                Toolchain,
                "Simulator runtimes",
                status,
                &value,
                detail,
            );
        }
    }
}
