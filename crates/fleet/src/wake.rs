//! Wake-on-LAN through another inventory machine on the same network.
//!
//! A sleeping machine can't be reached over Tailscale, so the magic packet is broadcast by
//! the host named in its `wake_via`, over SSH, after a review. Only a validated MAC address
//! reaches the script, as twelve hex digits.

use std::time::Duration;

use crate::desired::model::Host;
use crate::desired::py::{ps_quote, sh_quote};
use crate::desired::remote::{RemoteClient, RemoteError};

/// The limited broadcast address and the discard port Wake-on-LAN conventionally uses.
pub const BROADCAST: &str = "255.255.255.255";
pub const PORT: u16 = 9;

/// A MAC address in `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff` form, one separator style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddress([u8; 6]);

impl std::str::FromStr for MacAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("invalid mac_address: '{value}' (use six pairs of hex digits, like 'aa:bb:cc:dd:ee:ff')");
        let separator = match value.as_bytes().get(2) {
            Some(b':') => ':',
            Some(b'-') => '-',
            _ => return Err(invalid()),
        };
        let parts: Vec<&str> = value.split(separator).collect();
        if parts.len() != 6
            || parts
                .iter()
                .any(|p| p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(invalid());
        }
        let mut bytes = [0; 6];
        for (byte, part) in bytes.iter_mut().zip(parts) {
            *byte = u8::from_str_radix(part, 16).map_err(|_| invalid())?;
        }
        if bytes == [0; 6] || bytes == [0xff; 6] || bytes[0] & 1 == 1 {
            return Err(format!(
                "mac_address '{value}' isn't a single network interface's address"
            ));
        }
        Ok(Self(bytes))
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl MacAddress {
    /// Twelve lowercase hex digits: the only form the scripts receive.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Builds and broadcasts the packet with python3, then perl. A bash `/dev/udp` redirect
/// can't set `SO_BROADCAST`, so the kernel refuses a broadcast from it. On macOS, python3 is a
/// shim that may offer to install the developer tools, so it's used only when they exist.
const POSIX_WAKE: &str = r#"set -u
packet_python='import socket, sys
mac = bytes.fromhex(sys.argv[1])
packet = b"\xff" * 6 + mac * 16
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
s.sendto(packet, (sys.argv[2], int(sys.argv[3])))'
packet_perl='use Socket;
my $packet = ("\xff" x 6) . (pack("H12", $ARGV[0]) x 16);
socket(my $s, PF_INET, SOCK_DGRAM, getprotobyname("udp")) or die "socket: $!\n";
setsockopt($s, SOL_SOCKET, SO_BROADCAST, 1) or die "setsockopt: $!\n";
send($s, $packet, 0, pack_sockaddr_in($ARGV[2], inet_aton($ARGV[1]))) or die "send: $!\n";'
python=""
if [ "$(uname -s)" != Darwin ] || xcode-select -p >/dev/null 2>&1; then
  python="$(command -v python3 2>/dev/null || true)"
fi
if [ -n "$python" ]; then
  "$python" -c "$packet_python" "$mac" "$broadcast" "$port" && echo "sent with python3"
elif command -v perl >/dev/null 2>&1; then
  perl -e "$packet_perl" "$mac" "$broadcast" "$port" && echo "sent with perl"
else
  echo "neither python3 nor perl is available to send a broadcast" >&2
  exit 2
fi
"#;

const WINDOWS_WAKE: &str = r#"$ErrorActionPreference = 'Stop'
$bytes = [byte[]](0..5 | ForEach-Object { [Convert]::ToByte($mac.Substring($_ * 2, 2), 16) })
$packet = [byte[]]((@(0xFF) * 6) + (@($bytes) * 16))
$client = [Net.Sockets.UdpClient]::new()
try {
  $client.EnableBroadcast = $true
  [void]$client.Send($packet, $packet.Length, [Net.IPEndPoint]::new([Net.IPAddress]::Parse($broadcast), $port))
} finally {
  $client.Dispose()
}
Write-Output 'sent with UdpClient'
"#;

/// The script `via` runs to wake `mac`, for its shell.
pub fn wake_script(via: &Host, mac: &MacAddress) -> String {
    if via.is_windows() {
        format!(
            "$mac = {}\n$broadcast = {}\n$port = {PORT}\n{WINDOWS_WAKE}",
            ps_quote(&mac.hex()),
            ps_quote(BROADCAST)
        )
    } else {
        format!(
            "mac={}\nbroadcast={}\nport={PORT}\n{POSIX_WAKE}",
            sh_quote(&mac.hex()),
            sh_quote(BROADCAST)
        )
    }
}

/// Sends magic packets; tests replace it so no packet is ever sent.
pub trait WakeSender: Send + Sync {
    /// Blocks until `via` reports the packet sent. Call off the async runtime.
    fn send(&self, via: &Host, mac: &MacAddress) -> Result<String, RemoteError>;
}

impl WakeSender for RemoteClient {
    fn send(&self, via: &Host, mac: &MacAddress) -> Result<String, RemoteError> {
        self.run_script(via, &wake_script(via, mac), Some(Duration::from_secs(20)))
            .map(|out| out.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desired::process::{CommandError, CommandResult, CommandRunner, NonZeroExit};
    use crate::desired::remote::Programs;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mac_addresses_are_parsed_strictly() {
        for (text, expected) in [
            ("AA:bb:cc:01:02:03", "aa:bb:cc:01:02:03"),
            ("02-00-5e-10-00-01", "02:00:5e:10:00:01"),
        ] {
            assert_eq!(text.parse::<MacAddress>().unwrap().to_string(), expected);
        }
        for bad in [
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb:cc:dd:ee:ff:00",
            "aa-bb:cc-dd:ee-ff",
            "aabb.ccdd.eeff",
            "aa:bb:cc:dd:ee:gg",
            "aa:bb:cc:dd:ee:f",
            "aa:bb:cc:dd:ee:ff; reboot",
            "$(id):bb:cc:dd:ee:ff",
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff",
            "01:00:5e:00:00:01",
        ] {
            assert!(bad.parse::<MacAddress>().is_err(), "accepted {bad:?}");
        }
    }

    fn host(platform: &str) -> Host {
        let mode = if platform.starts_with("windows") {
            "windows-service"
        } else {
            "systemd"
        };
        let root = if platform.starts_with("windows") {
            "C:\\\\gh-runners\\\\acme"
        } else {
            "~/gh-runners/acme"
        };
        let label = if platform.starts_with("windows") {
            "windows-build"
        } else {
            "linux-build"
        };
        let text = format!(
            "[organization]\nname = \"acme\"\nurl = \"https://github.com/acme\"\nrunner_group = \"g\"\n\
             [[hosts]]\nid = \"relay\"\nplatform = \"{platform}\"\nssh = \"relay\"\nroot = \"{root}\"\n\
             service_mode = \"{mode}\"\nlabels = [\"fleet\", \"{label}\"]\nrunners = [{{ name = \"r\", directory = \"runner-01\" }}]\n"
        );
        crate::desired::inventory::parse_inventory(&text, &Default::default())
            .unwrap()
            .hosts
            .remove(0)
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(Vec<String>, String)>>);

    impl CommandRunner for Recorder {
        fn run(
            &self,
            command: &[String],
            input: Option<&str>,
            _: Duration,
            _: NonZeroExit,
        ) -> Result<CommandResult, CommandError> {
            self.0
                .lock()
                .unwrap()
                .push((command.to_vec(), input.unwrap_or_default().into()));
            Ok(CommandResult::ok("sent with python3\n"))
        }
    }

    #[test]
    fn the_packet_is_sent_from_the_relay_over_stdin_with_only_hex_digits() {
        let recorder = Arc::new(Recorder::default());
        let remote = RemoteClient::new(recorder.clone(), Duration::from_secs(30), Programs::default());
        let mac: MacAddress = "AA-BB-CC-01-02-03".parse().unwrap();
        assert_eq!(remote.send(&host("linux-arm64"), &mac).unwrap(), "sent with python3");
        let calls = recorder.0.lock().unwrap();
        let (argv, script) = &calls[0];
        assert_eq!(&argv[argv.len() - 3..], ["--", "relay", "bash -s"]);
        assert!(argv.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(!argv.join(" ").contains("aabbcc"), "the address travels on stdin");
        assert!(
            script.starts_with("mac=aabbcc010203\nbroadcast=255.255.255.255\nport=9\n"),
            "{script}"
        );
        assert!(script.contains("SO_BROADCAST"));
        let windows = wake_script(&host("windows-x64"), &mac);
        assert!(windows.starts_with("$mac = 'aabbcc010203'\n$broadcast = '255.255.255.255'\n$port = 9\n"));
        assert!(windows.contains("EnableBroadcast = $true"));
    }

    /// Runs the POSIX script with fake python3 and perl that print their arguments instead of
    /// sending anything.
    #[cfg(unix)]
    #[test]
    fn posix_script_prefers_python_then_perl_and_fails_without_either() {
        use std::os::unix::fs::PermissionsExt;
        let mac: MacAddress = "02:00:5e:10:00:01".parse().unwrap();
        let script = wake_script(&host("linux-x64"), &mac);
        let run = |tools: &[&str]| {
            let dir = tempfile::tempdir().unwrap();
            for tool in tools {
                let path = dir.path().join(tool);
                std::fs::write(&path, "#!/bin/sh\nshift 2\necho \"${0##*/} $*\"\n").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let uname = dir.path().join("uname");
            std::fs::write(&uname, "#!/bin/sh\necho Linux\n").unwrap();
            std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::process::Command::new("/bin/bash")
                .arg("-c")
                .arg(&script)
                .env("PATH", dir.path())
                .output()
                .unwrap()
        };
        let python = run(&["python3", "perl"]);
        assert_eq!(
            String::from_utf8_lossy(&python.stdout),
            "python3 02005e100001 255.255.255.255 9\nsent with python3\n"
        );
        let perl = run(&["perl"]);
        assert_eq!(
            String::from_utf8_lossy(&perl.stdout),
            "perl 02005e100001 255.255.255.255 9\nsent with perl\n"
        );
        let neither = run(&[]);
        assert_eq!(neither.status.code(), Some(2));
    }
}
