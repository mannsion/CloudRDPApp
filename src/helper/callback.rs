//! `cloudrdpapp --auth-callback <url>`: the desktop starts this for
//! `ms-appx-web:` addresses (see `crate::url_handler`). It checks the
//! address and forwards it to the running application over its helper
//! socket. Synchronous; it never initialises the GUI.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use super::server::{SOCKET_PREFIX, SOCKET_SUFFIX};
use super::{AuthCallback, AuthCallbackResponse};
use crate::auth::callback::validate_callback_url;
use crate::consts::APP_NAME;
use crate::paths;

/// The command-line flag.
pub const ARG: &str = "--auth-callback";

/// True when the command line is the desktop handing us a sign-in response.
pub fn is_callback_invocation(args: &[String]) -> bool {
    args.get(1).is_some_and(|a| a == ARG)
}

pub fn run(args: &[String]) -> ExitCode {
    let [_, _, url] = args else {
        eprintln!("usage: cloudrdpapp {ARG} <ms-appx-web://Microsoft.AAD.BrokerPlugin/…>");
        return ExitCode::from(2);
    };
    match forward(url, &paths::runtime_dir()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cloudrdpapp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Validate `url` and hand it to whichever running instance (helper socket
/// in `dir`) is waiting for its `state`.
pub fn forward(url: &str, dir: &Path) -> Result<(), String> {
    validate_callback_url(url).map_err(|err| format!("refusing the sign-in response: {err}."))?;
    let sockets = candidate_sockets(dir);
    if sockets.is_empty() {
        return Err(format!("{APP_NAME} is not running, so this sign-in response cannot be used. Start {APP_NAME} and sign in again."));
    }
    let mut refusal = None;
    for socket in sockets {
        match send(&socket, url) {
            Ok(response) if response.accepted => return Ok(()),
            Ok(response) => refusal = response.error.or(refusal),
            // A socket left behind by an instance that has exited.
            Err(err) => tracing::debug!("{}: {err}", socket.display()),
        }
    }
    Err(match refusal {
        Some(reason) => format!("the sign-in response was not accepted: {reason}."),
        None => format!("no running {APP_NAME} could be reached. Start {APP_NAME} and sign in again."),
    })
}

/// Helper sockets in `dir`, newest first.
fn candidate_sockets(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(SOCKET_PREFIX) && name.ends_with(SOCKET_SUFFIX)
        })
        .filter_map(|entry| {
            use std::os::unix::fs::FileTypeExt;
            let meta = entry.metadata().ok()?;
            meta.file_type().is_socket().then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), entry.path()))
        })
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, path)| path).collect()
}

fn send(socket: &Path, url: &str) -> Result<AuthCallbackResponse, String> {
    let mut stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    let timeout = Some(Duration::from_secs(10));
    stream.set_read_timeout(timeout).map_err(|err| err.to_string())?;
    stream.set_write_timeout(timeout).map_err(|err| err.to_string())?;
    let mut line = serde_json::to_string(&AuthCallback { auth_callback: url.to_owned() }).map_err(|err| err.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|err| err.to_string())?;
    let mut answer = String::new();
    BufReader::new(stream.take(256 * 1024))
        .read_line(&mut answer)
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&answer).map_err(|err| format!("unexpected answer: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::callback::CallbackHub;
    use crate::helper::server::HelperServer;
    use crate::util;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cloudrdpapp-callback-{}", util::random_token(6)))
    }

    #[test]
    fn recognises_the_flag() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_callback_invocation(&args(&["cloudrdpapp", "--auth-callback", "x"])));
        assert!(!is_callback_invocation(&args(&["cloudrdpapp", "--demo"])));
        assert!(!is_callback_invocation(&args(&["cloudrdpapp"])));
    }

    #[test]
    fn nothing_running() {
        let dir = test_dir();
        let err = forward("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1&state=s", &dir).unwrap_err();
        assert!(err.contains("not running"), "{err}");
        let err = forward("https://evil.example/?code=1&state=s", &dir).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaches_the_waiting_instance_among_several() {
        let dir = test_dir();
        paths::create_private_dir(&dir).unwrap();
        // A stale socket file from an instance that has exited.
        drop(std::os::unix::net::UnixListener::bind(dir.join("helper-1.sock")).unwrap());
        // An instance with nothing waiting, and the one that waits.
        let idle = HelperServer::start(&dir.join("x"), CallbackHub::new()).await.unwrap();
        std::fs::rename(idle.socket_path(), dir.join("helper-2.sock")).unwrap();
        let hub = CallbackHub::new();
        let server = HelperServer::start(&dir, hub.clone()).await.unwrap();
        let waiter = hub.expect("wanted");

        let url = "ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=c&state=wanted".to_owned();
        let (d, u) = (dir.clone(), url.clone());
        tokio::task::spawn_blocking(move || forward(&u, &d)).await.unwrap().unwrap();
        assert_eq!(waiter.wait(Duration::from_secs(2)).await.unwrap(), url);

        // Nobody waits any more: the refusal reason comes back.
        let (d, u) = (dir.clone(), url.clone());
        let err = tokio::task::spawn_blocking(move || forward(&u, &d)).await.unwrap().unwrap_err();
        assert!(err.contains("not accepted") && err.contains("no sign-in"), "{err}");
        drop(server);
        drop(idle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
