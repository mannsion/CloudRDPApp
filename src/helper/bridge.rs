//! Helper mode: the process FreeRDP spawns. Synchronous and dependency-light;
//! it never initialises the GUI.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use super::{NavigateRequest, NavigateResponse};
use crate::consts::{APP_NAME, ENV_HELPER_SECRET, ENV_HELPER_SESSION, ENV_HELPER_SOCKET};

const ARG_IN: &str = "--cmdInFd=";
const ARG_OUT: &str = "--cmdOutFd=";
const HELPER_NAME: &str = concat!("cloudrdpapp-helper/", env!("CARGO_PKG_VERSION"));

/// True when the command line is FreeRDP starting us as its helper.
pub fn is_helper_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a.starts_with(ARG_IN)) && args.iter().any(|a| a.starts_with(ARG_OUT))
}

/// Decode a handle exported by WinPR (`winpr_exportHandleToString`): a type
/// tag (`P` for pipe) followed by the file descriptor in hexadecimal.
pub fn parse_handle(value: &str) -> Option<RawFd> {
    let digits = value.strip_prefix('P')?;
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let fd = RawFd::from_str_radix(digits, 16).ok()?;
    (fd > 2).then_some(fd)
}

fn handle_arg(args: &[String], prefix: &str) -> Option<RawFd> {
    args.iter().find_map(|a| a.strip_prefix(prefix)).and_then(parse_handle)
}

pub fn run(args: &[String]) -> ExitCode {
    let (Some(in_fd), Some(out_fd)) = (handle_arg(args, ARG_IN), handle_arg(args, ARG_OUT)) else {
        eprintln!("cloudrdpapp helper: malformed --cmdInFd/--cmdOutFd arguments");
        return ExitCode::from(2);
    };
    // SAFETY: FreeRDP created these two pipe ends for this process and passed
    // their numbers on the command line; nothing else here owns them.
    let (input, output) = unsafe { (File::from_raw_fd(in_fd), File::from_raw_fd(out_fd)) };
    let forwarder = SocketForwarder::from_env();
    match serve(BufReader::new(input), output, &forwarder) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cloudrdpapp helper: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Where navigate requests go. Abstracted so the protocol loop can be tested
/// without a socket.
pub trait Forward {
    fn navigate(&self, url: &str, redirect_uri: &str, title: &str, timeout_ms: u64) -> NavigateResponse;
}

/// Serve the JSON-RPC protocol until FreeRDP says `exit` or closes the pipe.
pub fn serve<R: BufRead, W: Write>(reader: R, mut writer: W, forwarder: &dyn Forward) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        // Lines that are not JSON are ignored, as the protocol specifies.
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let reply = match method {
            "hello" => Some(json!({ "result": { "protocol_version": 1, "helper": HELPER_NAME } })),
            "navigate" => {
                let text = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
                let timeout_ms = match params.get("timeout_ms").and_then(Value::as_u64) {
                    Some(0) | None => 180_000,
                    Some(ms) => ms,
                };
                let (url, redirect_uri) = (text("url"), text("redirect_uri"));
                let response = if url.is_empty() || redirect_uri.is_empty() {
                    NavigateResponse::error("navigate needs url and redirect_uri")
                } else {
                    forwarder.navigate(&url, &redirect_uri, &text("title"), timeout_ms)
                };
                Some(match response {
                    NavigateResponse { redirect_url: Some(redirect_url), .. } => {
                        json!({ "result": { "status": "ok", "redirect_url": redirect_url } })
                    }
                    NavigateResponse { error, .. } => {
                        json!({ "error": { "code": 1, "message": error.unwrap_or_else(|| "failed".to_owned()) } })
                    }
                })
            }
            "shutdown" => Some(json!({ "result": Value::Null })),
            "exit" => break,
            // `cancel` has nothing to cancel: navigate is answered synchronously.
            "cancel" => None,
            _ => id.is_some().then(|| json!({ "error": { "code": -32601, "message": "method not found" } })),
        };

        if let (Some(mut reply), Some(id)) = (reply, id) {
            reply["jsonrpc"] = json!("2.0");
            reply["id"] = id;
            let mut text = reply.to_string();
            text.push('\n');
            writer.write_all(text.as_bytes())?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Forwards navigate requests to the application over its Unix socket.
pub struct SocketForwarder {
    socket: Option<String>,
    session: String,
    secret: String,
}

impl SocketForwarder {
    pub fn from_env() -> Self {
        Self {
            socket: std::env::var(ENV_HELPER_SOCKET).ok().filter(|v| !v.is_empty()),
            session: std::env::var(ENV_HELPER_SESSION).unwrap_or_default(),
            secret: std::env::var(ENV_HELPER_SECRET).unwrap_or_default(),
        }
    }

    pub fn new(socket: impl Into<String>, session: impl Into<String>, secret: impl Into<String>) -> Self {
        Self { socket: Some(socket.into()), session: session.into(), secret: secret.into() }
    }

    fn exchange(&self, request: &NavigateRequest, timeout: Duration) -> Result<NavigateResponse, String> {
        let socket = self
            .socket
            .as_deref()
            .ok_or_else(|| format!("{APP_NAME} is not running (no helper socket in the environment)"))?;
        let mut stream = UnixStream::connect(socket).map_err(|err| format!("cannot reach {APP_NAME}: {err}"))?;
        stream.set_read_timeout(Some(timeout)).map_err(|err| err.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(|err| err.to_string())?;
        let mut payload = serde_json::to_string(request).map_err(|err| err.to_string())?;
        payload.push('\n');
        stream.write_all(payload.as_bytes()).map_err(|err| err.to_string())?;

        let mut answer = String::new();
        BufReader::new((&stream).take(1024 * 1024))
            .read_line(&mut answer)
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => super::ERROR_TIMEOUT.to_owned(),
                _ => err.to_string(),
            })?;
        if answer.trim().is_empty() {
            return Err(format!("{APP_NAME} closed the connection without answering"));
        }
        serde_json::from_str(&answer).map_err(|err| format!("unreadable answer from {APP_NAME}: {err}"))
    }
}

impl Forward for SocketForwarder {
    fn navigate(&self, url: &str, redirect_uri: &str, title: &str, timeout_ms: u64) -> NavigateResponse {
        let request = NavigateRequest {
            session: self.session.clone(),
            secret: self.secret.clone(),
            url: url.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
            title: title.to_owned(),
            timeout_ms,
        };
        // Give the application the whole budget, plus a little slack for the round trip.
        match self.exchange(&request, Duration::from_millis(timeout_ms) + Duration::from_secs(5)) {
            Ok(response) => response,
            Err(message) => NavigateResponse::error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Scripted {
        answer: NavigateResponse,
        seen: RefCell<Vec<(String, String, u64)>>,
    }

    impl Forward for Scripted {
        fn navigate(&self, url: &str, redirect_uri: &str, _title: &str, timeout_ms: u64) -> NavigateResponse {
            self.seen.borrow_mut().push((url.to_owned(), redirect_uri.to_owned(), timeout_ms));
            self.answer.clone()
        }
    }

    fn run_script(input: &str, answer: NavigateResponse) -> (Vec<Value>, Vec<(String, String, u64)>) {
        let forwarder = Scripted { answer, seen: RefCell::new(Vec::new()) };
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, &forwarder).unwrap();
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (replies, forwarder.seen.into_inner())
    }

    #[test]
    fn recognises_helper_invocations() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_helper_invocation(&args(&["cloudrdpapp", "--cmdInFd=P5", "--cmdOutFd=P8"])));
        assert!(!is_helper_invocation(&args(&["cloudrdpapp"])));
        assert!(!is_helper_invocation(&args(&["cloudrdpapp", "--cmdInFd=P5"])));
    }

    #[test]
    fn decodes_winpr_handles() {
        assert_eq!(parse_handle("P5"), Some(5));
        assert_eq!(parse_handle("P1f"), Some(31));
        assert_eq!(parse_handle("5"), None, "the type tag is required");
        assert_eq!(parse_handle("P"), None);
        assert_eq!(parse_handle("Pzz"), None);
        assert_eq!(parse_handle("P1"), None, "standard streams are never the protocol pipes");
    }

    #[test]
    fn full_session_as_freerdp_drives_it() {
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1,"client":"freerdp"}}"#, "\n",
            "this line is not json\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"navigate","params":{"title":"Sign in","url":"https://127.0.0.1:1/common/oauth2/v2.0/authorize?x=1","redirect_uri":"ms-appx-web://Microsoft.AAD.BrokerPlugin/abc","timeout_ms":0}}"#, "\n",
            r#"{"jsonrpc":"2.0","method":"cancel"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#, "\n",
            r#"{"jsonrpc":"2.0","method":"exit"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"hello"}"#, "\n",
        );
        let (replies, seen) = run_script(script, NavigateResponse::ok("ms-appx-web://x/?code=c&state=s"));
        assert_eq!(replies.len(), 3, "nothing is answered after exit");

        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[0]["jsonrpc"], "2.0");
        assert_eq!(replies[0]["result"]["protocol_version"], 1);

        assert_eq!(replies[1]["id"], 2);
        assert_eq!(replies[1]["result"]["status"], "ok");
        assert_eq!(replies[1]["result"]["redirect_url"], "ms-appx-web://x/?code=c&state=s");

        assert_eq!(replies[2]["id"], 3);
        assert!(replies[2]["result"].is_null());
        assert!(replies[2].get("error").is_none());

        // timeout_ms 0 means "the helper's default".
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, "ms-appx-web://Microsoft.AAD.BrokerPlugin/abc");
        assert_eq!(seen[0].2, 180_000);
    }

    #[test]
    fn failures_use_the_protocol_error_shape() {
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"navigate","params":{"url":"https://x/a","redirect_uri":"http://localhost:1/"}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"navigate","params":{"url":"","redirect_uri":""}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus"}"#, "\n",
        );
        let (replies, seen) = run_script(script, NavigateResponse::error("user_cancelled"));
        assert_eq!(replies[0]["error"]["message"], "user_cancelled");
        assert_eq!(replies[0]["error"]["code"], 1);
        assert!(replies[0].get("result").is_none());
        assert!(replies[1]["error"]["message"].as_str().unwrap().contains("needs url"));
        assert_eq!(replies[2]["error"]["code"], -32601);
        assert_eq!(seen.len(), 1, "the malformed navigate is not forwarded");
    }

    #[test]
    fn forwarder_without_socket_reports_the_app_is_not_running() {
        let forwarder = SocketForwarder { socket: None, session: String::new(), secret: String::new() };
        let response = forwarder.navigate("https://x/", "http://localhost/", "", 1000);
        assert!(response.error.unwrap().contains("not running"));
    }
}
