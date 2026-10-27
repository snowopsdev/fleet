//! Optional OpenSSH connection reuse for read-only monitoring sessions.
//!
//! Each machine is read over SSH every minute. With reuse on, the first read opens a control
//! master (`ControlMaster=auto`) that later reads multiplex over for up to
//! [`CONTROL_PERSIST_SECS`] after the last one, instead of a full handshake each time. The
//! sockets live in a private per-user directory with a short path, since macOS limits socket
//! paths to 104 bytes. Reviewed changes never use it: they run through a separate client
//! without these options. `tailscale ssh` takes no OpenSSH options and is unaffected.

use std::path::{Path, PathBuf};

/// How long an idle control master stays open after its last session.
pub const CONTROL_PERSIST_SECS: u64 = 120;

/// macOS's `sockaddr_un` path limit, including the terminating NUL.
const MAX_SOCKET_PATH: usize = 104;
/// `%C` expands to a 40-character hash of the local host, remote host, port and user.
const HASH_LEN: usize = 40;
/// While becoming master, OpenSSH binds a temporary `<path>.<16 random characters>` first.
const TEMPORARY_SUFFIX_LEN: usize = 17;

/// A verified private directory for OpenSSH control sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlDir {
    path: PathBuf,
}

impl ControlDir {
    /// `/tmp/runner-dashboard-<uid>`: short enough for socket paths, and per user.
    pub fn default_path() -> PathBuf {
        #[cfg(unix)]
        let user = rustix::process::getuid().as_raw().to_string();
        #[cfg(not(unix))]
        let user = "user".to_string();
        PathBuf::from(format!("/tmp/runner-dashboard-{user}"))
    }

    /// Creates the directory with mode 0700 if needed, then checks that it is a real directory
    /// owned by this user that no one else can enter. Anything else is refused, so another
    /// user can't plant or read sockets.
    ///
    /// # Errors
    ///
    /// A sentence explaining why connection reuse is unavailable.
    pub fn prepare(path: &Path) -> Result<Self, String> {
        let text = path.to_str().unwrap_or_default();
        if !path.is_absolute()
            || text.is_empty()
            || !text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        {
            return Err(format!(
                "The SSH connection directory {} must be an absolute path of letters, digits and ./_-.",
                path.display()
            ));
        }
        if text.len() + 1 + HASH_LEN + TEMPORARY_SUFFIX_LEN >= MAX_SOCKET_PATH {
            return Err(format!(
                "The SSH connection directory {} is too long for a control socket path.",
                path.display()
            ));
        }
        Self::create_private(path)?;
        Ok(Self { path: path.into() })
    }

    #[cfg(unix)]
    fn create_private(path: &Path) -> Result<(), String> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("Couldn't create {}: {e}", path.display())),
        }
        let metadata =
            std::fs::symlink_metadata(path).map_err(|e| format!("Couldn't check {}: {e}", path.display()))?;
        let uid = rustix::process::getuid().as_raw();
        if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{} isn't a private directory owned by you, so SSH connections aren't shared. Remove it and try again.",
                path.display()
            ));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn create_private(_: &Path) -> Result<(), String> {
        Err("Sharing SSH connections needs a Unix control socket.".into())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// OpenSSH options added ahead of the target, after the usual hardening options. Dead
    /// masters are noticed within about 30 seconds, so a dropped network doesn't keep
    /// failing reads.
    pub fn options(&self) -> Vec<String> {
        [
            "ControlMaster=auto".to_string(),
            format!("ControlPersist={CONTROL_PERSIST_SECS}"),
            // `%%` would be needed for a literal percent sign; `prepare` allows none.
            format!("ControlPath={}/%C", self.path.display()),
            "ServerAliveInterval=10".into(),
            "ServerAliveCountMax=3".into(),
        ]
        .into_iter()
        .flat_map(|option| ["-o".to_string(), option])
        .collect()
    }

    /// Removes this user's sockets, so no later session joins an existing master. Masters
    /// left without a socket exit once [`CONTROL_PERSIST_SECS`] pass without sessions.
    pub fn close_all(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let uid = rustix::process::getuid().as_raw();
            let Ok(entries) = std::fs::read_dir(&self.path) else {
                return;
            };
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata()
                    && metadata.file_type().is_socket()
                    && metadata.uid() == uid
                    && let Err(e) = std::fs::remove_file(entry.path())
                {
                    tracing::warn!("couldn't remove SSH control socket {}: {e}", entry.path().display());
                }
            }
        }
    }
}

/// Whether an SSH failure came from a broken control socket or master rather than the
/// machine, so the read is worth repeating once without reuse. OpenSSH already replaces a
/// socket whose master is gone (`connect` refused); these are the other mux failures.
pub(crate) fn is_control_failure(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "mux_client",
        "muxclient",
        "control socket",
        "controlsocket",
        "master hello",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_keep_paths_short_and_reject_unsafe_directories() {
        let dir = tempfile::Builder::new().prefix("rd").tempdir_in("/tmp").unwrap();
        let control = ControlDir::prepare(&dir.path().join("ssh")).unwrap();
        let options = control.options();
        assert_eq!(
            options,
            [
                "-o".to_string(),
                "ControlMaster=auto".into(),
                "-o".into(),
                "ControlPersist=120".into(),
                "-o".into(),
                format!("ControlPath={}/ssh/%C", dir.path().display()),
                "-o".into(),
                "ServerAliveInterval=10".into(),
                "-o".into(),
                "ServerAliveCountMax=3".into(),
            ]
        );
        for path in ["relative/ssh", "/tmp/has space", "/tmp/100%", "/tmp/x;rm"] {
            assert!(ControlDir::prepare(Path::new(path)).is_err(), "{path}");
        }
        let long = format!("/tmp/{}", "a".repeat(60));
        assert!(ControlDir::prepare(Path::new(&long)).unwrap_err().contains("too long"));
        assert!(
            ControlDir::default_path().to_str().unwrap().len() + 1 + HASH_LEN + TEMPORARY_SUFFIX_LEN < MAX_SOCKET_PATH
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_or_symlinked_directories_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new().prefix("rd").tempdir_in("/tmp").unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ControlDir::prepare(&open).unwrap_err().contains("private directory"));
        let private = dir.path().join("private");
        ControlDir::prepare(&private).unwrap();
        assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(ControlDir::prepare(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn closing_removes_only_sockets() {
        let dir = tempfile::Builder::new().prefix("rd").tempdir_in("/tmp").unwrap();
        let control = ControlDir::prepare(&dir.path().join("ssh")).unwrap();
        let socket = control.path().join("0123abcd");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let note = control.path().join("note");
        std::fs::write(&note, "kept").unwrap();
        control.close_all();
        assert!(!socket.exists());
        assert!(note.exists());
    }

    #[test]
    fn control_failures_are_told_apart_from_machine_failures() {
        assert!(is_control_failure(
            "mux_client_request_session: read from master failed: Broken pipe"
        ));
        assert!(is_control_failure(
            "ControlSocket /tmp/x already exists, disabling multiplexing"
        ));
        assert!(!is_control_failure(
            "ssh: connect to host mini port 22: Operation timed out"
        ));
        assert!(!is_control_failure("Permission denied (publickey)."));
    }
}
