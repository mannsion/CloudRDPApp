//! Application side of the helper bridge: a Unix socket (mode 0600, in a
//! 0700 directory) that receives navigate requests from the bridge processes
//! FreeRDP spawns, and broker sign-in responses from `--auth-callback`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{AuthCallback, AuthCallbackResponse, ERROR_TIMEOUT, NavigateRequest, NavigateResponse};
use crate::auth::callback::CallbackHub;
use crate::paths;
use crate::util;

/// Answers one navigate request for one session.
pub type NavigateHandler =
    Arc<dyn Fn(NavigateRequest) -> Pin<Box<dyn Future<Output = NavigateResponse> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct HelperServer {
    inner: Arc<Inner>,
}

struct Inner {
    socket_path: PathBuf,
    sessions: Mutex<HashMap<String, Registered>>,
    callbacks: CallbackHub,
}

struct Registered {
    secret: String,
    handler: NavigateHandler,
}

/// Credentials a FreeRDP child needs in its environment to reach its handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub session: String,
    pub secret: String,
}

impl HelperServer {
    /// Bind the socket inside `dir` (created private to the user). Sign-in
    /// callbacks are handed to `callbacks`. Must be called inside a Tokio
    /// runtime.
    pub async fn start(dir: &Path, callbacks: CallbackHub) -> Result<Self, String> {
        paths::create_private_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let socket_path = dir.join(format!("{SOCKET_PREFIX}{}{SOCKET_SUFFIX}", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).map_err(|err| format!("{}: {err}", socket_path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
        }

        let server = Self { inner: Arc::new(Inner { socket_path, sessions: Mutex::new(HashMap::new()), callbacks }) };
        let accept = server.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = accept.clone();
                        tokio::spawn(async move {
                            if let Err(err) = server.serve(stream).await {
                                tracing::debug!("helper connection ended: {err}");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!("helper socket accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        Ok(server)
    }

    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    /// Register a handler for a new session and return its credentials.
    pub fn register(&self, handler: NavigateHandler) -> Registration {
        let registration = Registration { session: util::random_token(12), secret: util::random_token(32) };
        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(registration.session.clone(), Registered { secret: registration.secret.clone(), handler });
        registration
    }

    pub fn unregister(&self, session: &str) {
        self.inner.sessions.lock().unwrap().remove(session);
    }

    async fn serve(&self, stream: UnixStream) -> Result<(), String> {
        let (read_half, mut write_half) = stream.into_split();
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            BufReader::new(read_half.take(256 * 1024)).read_line(&mut line),
        )
        .await
        .map_err(|_| "timed out waiting for the request".to_owned())?
        .map_err(|err| err.to_string())?;
        if read == 0 {
            return Ok(());
        }
        let mut payload = if let Ok(callback) = serde_json::from_str::<AuthCallback>(&line) {
            let response = match self.inner.callbacks.deliver(&callback.auth_callback) {
                Ok(()) => AuthCallbackResponse { accepted: true, error: None },
                Err(err) => AuthCallbackResponse { accepted: false, error: Some(err) },
            };
            serde_json::to_string(&response)
        } else {
            let response = match serde_json::from_str::<NavigateRequest>(&line) {
                Ok(request) => self.dispatch(request).await,
                Err(err) => NavigateResponse::error(format!("malformed request: {err}")),
            };
            serde_json::to_string(&response)
        }
        .map_err(|err| err.to_string())?;
        payload.push('\n');
        write_half.write_all(payload.as_bytes()).await.map_err(|err| err.to_string())?;
        write_half.shutdown().await.map_err(|err| err.to_string())
    }

    async fn dispatch(&self, request: NavigateRequest) -> NavigateResponse {
        let handler = {
            let sessions = self.inner.sessions.lock().unwrap();
            match sessions.get(&request.session) {
                Some(registered) if constant_time_eq(registered.secret.as_bytes(), request.secret.as_bytes()) => {
                    registered.handler.clone()
                }
                _ => return NavigateResponse::error("unknown session"),
            }
        };
        let budget = Duration::from_millis(if request.timeout_ms == 0 { 180_000 } else { request.timeout_ms });
        match tokio::time::timeout(budget, handler(request)).await {
            Ok(response) => response,
            Err(_) => NavigateResponse::error(ERROR_TIMEOUT),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Socket file names: `helper-<pid>.sock` in the runtime directory. The
/// `--auth-callback` process tries every one of them.
pub const SOCKET_PREFIX: &str = "helper-";
pub const SOCKET_SUFFIX: &str = ".sock";

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::bridge::{Forward, SocketForwarder};

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cloudrdpapp-helper-{}", util::random_token(6)))
    }

    fn echo_handler() -> NavigateHandler {
        Arc::new(|request: NavigateRequest| {
            Box::pin(async move { NavigateResponse::ok(format!("{}?code=issued&state=from-{}", request.redirect_uri, request.url)) })
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_reaches_its_session_handler() {
        let dir = test_dir();
        let server = HelperServer::start(&dir, CallbackHub::new()).await.unwrap();
        let registration = server.register(echo_handler());
        let socket = server.socket_path().to_str().unwrap().to_owned();

        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);

        // The bridge is synchronous code, as in the real helper process.
        let reg = registration.clone();
        let sock = socket.clone();
        let answer = tokio::task::spawn_blocking(move || {
            SocketForwarder::new(sock, reg.session, reg.secret).navigate("URL", "http://localhost:9/", "t", 5_000)
        })
        .await
        .unwrap();
        assert_eq!(answer.redirect_url.as_deref(), Some("http://localhost:9/?code=issued&state=from-URL"));

        // A wrong secret is refused.
        let reg = registration.clone();
        let sock = socket.clone();
        let refused = tokio::task::spawn_blocking(move || {
            SocketForwarder::new(sock, reg.session, "wrong").navigate("URL", "http://localhost:9/", "t", 5_000)
        })
        .await
        .unwrap();
        assert_eq!(refused.error.as_deref(), Some("unknown session"));

        // After unregistering, the session is gone.
        server.unregister(&registration.session);
        let reg = registration.clone();
        let gone = tokio::task::spawn_blocking(move || {
            SocketForwarder::new(socket, reg.session, reg.secret).navigate("URL", "http://localhost:9/", "t", 5_000)
        })
        .await
        .unwrap();
        assert_eq!(gone.error.as_deref(), Some("unknown session"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_handlers_time_out() {
        let dir = test_dir();
        let server = HelperServer::start(&dir, CallbackHub::new()).await.unwrap();
        let handler: NavigateHandler = Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                NavigateResponse::ok("never")
            })
        });
        let registration = server.register(handler);
        let socket = server.socket_path().to_str().unwrap().to_owned();
        let answer = tokio::task::spawn_blocking(move || {
            SocketForwarder::new(socket, registration.session, registration.secret).navigate("u", "r", "", 150)
        })
        .await
        .unwrap();
        assert_eq!(answer.error.as_deref(), Some(ERROR_TIMEOUT));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
