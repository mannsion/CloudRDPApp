//! Locating FreeRDP and interpreting what it reports.

use std::path::{Path, PathBuf};

use crate::config::ClientKind;
use crate::consts::MIN_FREERDP_VERSION;

pub type Version = (u32, u32, u32);

/// A FreeRDP client found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub kind: ClientKind,
    pub path: PathBuf,
    pub version: Option<Version>,
}

impl Client {
    /// True when this build has the sign-in helper protocol the app needs.
    pub fn supports_helper(&self) -> bool {
        self.version.map(|v| v >= MIN_FREERDP_VERSION).unwrap_or(false)
    }

    pub fn version_string(&self) -> String {
        match self.version {
            Some((a, b, c)) => format!("{a}.{b}.{c}"),
            None => "unknown version".to_owned(),
        }
    }
}

/// Executable names to try, most specific first.
pub fn candidate_names(kind: ClientKind) -> &'static [&'static str] {
    match kind {
        ClientKind::Sdl => &["sdl-freerdp3", "sdl3-freerdp", "sdl-freerdp"],
        ClientKind::X11 => &["xfreerdp3", "xfreerdp"],
    }
}

/// Find a client: the configured path if set, otherwise search `PATH`.
pub fn find_binary(kind: ClientKind, configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        return is_executable(&path).then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for name in candidate_names(kind) {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Find a client and ask it for its version. Blocks briefly.
pub fn detect(kind: ClientKind, configured: &str) -> Option<Client> {
    let path = find_binary(kind, configured)?;
    let version = std::process::Command::new(&path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| parse_version(&String::from_utf8_lossy(&out.stdout)));
    Some(Client { kind, path, version })
}

/// Parse `This is FreeRDP version 3.33.1 (65bcca1913)`.
pub fn parse_version(output: &str) -> Option<Version> {
    let rest = &output[output.find("version ")? + "version ".len()..];
    let token: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = token.split('.').filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// What a FreeRDP exit code means, in words a user can act on. The SDL and
/// X11 clients share this numbering.
pub fn describe_exit(code: i32) -> Option<&'static str> {
    Some(match code {
        0 | 1 | 2 | 11 => return None, // normal ends: success, disconnect, logoff, user disconnect
        3 => "The session ended after being idle for too long.",
        4 => "The session reached its time limit.",
        5 => "The session was taken over by another connection.",
        7 => "The connection was denied.",
        9 => "This account is not allowed to sign in to the remote computer.",
        10 => "The remote computer asked for fresh credentials.",
        128 => "FreeRDP rejected the connection options. Check the extra arguments in Settings.",
        131 => "The connection failed.",
        132 => "Authentication failed.",
        133 => "The client and server could not agree on a security protocol.",
        134 => "Sign-in to the remote computer failed.",
        135 => "The account is locked out.",
        136 => "The connection could not be prepared (display or option problem).",
        139 | 140 => "The gateway or host name could not be resolved.",
        141 => "The remote computer could not be reached.",
        143 => "The secure (TLS) connection could not be established.",
        144 => "This account lacks the privileges to connect.",
        145 => "The connection was cancelled.",
        147 => "The gateway connection failed. The sign-in token may have been rejected.",
        148 | 152 => "The password has expired.",
        149 => "The password must be changed before signing in.",
        151 => "The account is disabled.",
        154 => "The user name or password is wrong.",
        155 => "Access was denied.",
        156..=158 => "The account is restricted from signing in here.",
        159 => "Credentials are missing.",
        160 => "The remote computer is still starting. Try again in a minute.",
        161 => "The remote computer requires Network Level Authentication.",
        _ => return None,
    })
}

/// Look for a known failure in FreeRDP's log and explain it.
pub fn explain_log_line(line: &str) -> Option<&'static str> {
    const MONITOR_OVERLAP_MSG: &str =
        "The monitor layout is not usable for a multi-monitor session at this display scale. Use a single monitor or a whole-number scale.";
    const KNOWN: &[(&str, &str)] = &[
        (
            "ERRCONNECT_TARGET_BOOTING",
            "The desktop was switched off and is starting up. This takes a few minutes; FreeRDP keeps retrying, or connect again shortly.",
        ),
        (
            "ERRCONNECT_HYBRID_REQUIRED_BY_SERVER",
            "This host requires password sign-in (NLA). Set \"Session host sign-in\" to Password for this desktop.",
        ),
        (
            "ERRCONNECT_LOGON_FAILURE",
            "The remote computer rejected the sign-in.",
        ),
        (
            "ERRCONNECT_AUTHENTICATION_FAILED",
            "Authentication to the remote computer failed.",
        ),
        (
            "ERRCONNECT_PRE_CONNECT_FAILED",
            "FreeRDP could not set up the display. Try the window mode, a single monitor, or the X11 video backend.",
        ),
        (
            "ERRCONNECT_ACCESS_DENIED",
            "Access to this desktop was denied by the service.",
        ),
        (
            "ERRCONNECT_DNS_NAME_NOT_FOUND",
            "The gateway name could not be resolved. Check the network connection.",
        ),
        (
            "ERRCONNECT_TLS_CONNECT_FAILED",
            "The secure connection failed; the certificate may have been rejected.",
        ),
        (
            "ERRCONNECT_CONNECT_TRANSPORT_FAILED",
            "The gateway refused the connection. The sign-in token may have been rejected.",
        ),
        (
            "ERRCONNECT_ACTIVATION_TIMEOUT",
            "The remote computer did not finish starting the session in time.",
        ),
        (
            "Unable to obtain access token",
            "FreeRDP could not get a sign-in token for the remote computer.",
        ),
        ("monitor_overlaps", MONITOR_OVERLAP_MSG),
        ("monitor_operlaps", MONITOR_OVERLAP_MSG),
    ];
    KNOWN.iter().find(|(needle, _)| line.contains(needle)).map(|(_, text)| *text)
}

/// A FreeRDP log line that marks an established session.
pub fn is_connected_marker(line: &str) -> bool {
    line.contains("Local framebuffer format") || line.contains("Remote framebuffer format")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("This is FreeRDP version 3.33.1 (65bcca1913)\n"), Some((3, 33, 1)));
        assert_eq!(parse_version("This is FreeRDP version 3.5.0-dev (abc)"), Some((3, 5, 0)));
        assert_eq!(parse_version("This is FreeRDP version 2.11"), Some((2, 11, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn helper_support_needs_3_32() {
        let mut client = Client { kind: ClientKind::Sdl, path: "/usr/bin/sdl-freerdp3".into(), version: Some((3, 33, 1)) };
        assert!(client.supports_helper());
        assert_eq!(client.version_string(), "3.33.1");
        client.version = Some((3, 31, 9));
        assert!(!client.supports_helper());
        client.version = None;
        assert!(!client.supports_helper());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(describe_exit(0), None);
        assert_eq!(describe_exit(11), None);
        assert!(describe_exit(131).unwrap().contains("failed"));
        assert!(describe_exit(161).unwrap().contains("Network Level Authentication"));
        assert_eq!(describe_exit(9999), None);
    }

    #[test]
    fn explains_known_failures() {
        let line = "[ERROR][com.freerdp.core] - [nego_connect]: ERRCONNECT_HYBRID_REQUIRED_BY_SERVER [0x0002001E]";
        assert!(explain_log_line(line).unwrap().contains("Password"));
        assert_eq!(explain_log_line("[INFO] all good"), None);
        assert!(is_connected_marker("[INFO][com.freerdp.gdi] - [gdi_init_ex]: Local framebuffer format  PIXEL_FORMAT_BGRX32"));
        assert!(!is_connected_marker("[INFO] connecting"));
    }

    #[test]
    fn finds_binaries() {
        // A configured path that does not exist yields nothing rather than a PATH search.
        assert_eq!(find_binary(ClientKind::Sdl, "/nonexistent/sdl-freerdp3"), None);
        assert_eq!(find_binary(ClientKind::Sdl, "/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
    }
}
