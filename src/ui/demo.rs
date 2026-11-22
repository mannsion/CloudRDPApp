//! Demo mode (`--demo` or `CLOUDRDPAPP_DEMO=1`): a fake account and feed and
//! simulated connections, with no network, no keyring and no FreeRDP. It
//! exists so the GUI can be exercised and screenshotted without an account.
//!
//! `CLOUDRDPAPP_DEMO_FEED=empty|fail` shows the empty and failed feed states;
//! `CLOUDRDPAPP_DEMO_FREERDP=missing|old` shows the FreeRDP banner.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::auth::{Account, AuthError, OAuthError};
use crate::backend::{BackendError, Clients};
use crate::config::ClientKind;
use crate::feed::{Feed, FeedError, Resource, ResourceKind};
use crate::rdp::freerdp::Client;
use crate::rdp::session::{EventSink, SessionEvent};
use crate::util;

/// Resource id of the demo desktop whose connection fails.
pub const FAILING_ID: &str = "demo-lab";
/// Resource id of the demo desktop that signs in with a password.
pub const PASSWORD_ID: &str = "demo-legacy";

pub fn requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--demo") || std::env::var("CLOUDRDPAPP_DEMO").is_ok_and(|v| v == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedMode {
    Normal,
    Empty,
    Fail,
}

pub struct Demo {
    feed_mode: FeedMode,
    freerdp: Option<&'static str>,
    log_dir: PathBuf,
    passwords: Mutex<HashMap<String, String>>,
    fetches: Mutex<u32>,
}

impl Demo {
    pub fn from_env() -> Self {
        let feed_mode = match std::env::var("CLOUDRDPAPP_DEMO_FEED").as_deref() {
            Ok("empty") => FeedMode::Empty,
            Ok("fail") => FeedMode::Fail,
            _ => FeedMode::Normal,
        };
        let freerdp = match std::env::var("CLOUDRDPAPP_DEMO_FREERDP").as_deref() {
            Ok("missing") => Some("missing"),
            Ok("old") => Some("old"),
            _ => None,
        };
        let demo = Self::new(feed_mode, freerdp);
        // Logs of earlier demo runs are of no use.
        let _ = std::fs::remove_dir_all(&demo.log_dir);
        demo
    }

    pub fn new(feed_mode: FeedMode, freerdp: Option<&'static str>) -> Self {
        Self {
            feed_mode,
            freerdp,
            // SAFETY: getuid cannot fail.
            log_dir: std::env::temp_dir().join(format!("cloudrdpapp-demo-{}", unsafe { libc::getuid() })),
            passwords: Mutex::new(HashMap::new()),
            fetches: Mutex::new(0),
        }
    }

    pub fn account() -> Account {
        Account {
            tenant_id: "00000000-0000-0000-0000-00000000demo".into(),
            object_id: "11111111-1111-1111-1111-11111111demo".into(),
            username: "alex@example.com".into(),
            name: Some("Alex Demo".into()),
        }
    }

    pub fn clients(&self) -> Clients {
        let version = match self.freerdp {
            Some("missing") => return Clients::default(),
            Some(_) => Some((3, 10, 2)),
            None => Some((3, 33, 1)),
        };
        Clients {
            sdl: Some(Client { kind: ClientKind::Sdl, path: "/usr/bin/sdl-freerdp3".into(), version }),
            x11: Some(Client { kind: ClientKind::X11, path: "/usr/bin/xfreerdp3".into(), version }),
        }
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Simulated browser sign-in; slow enough to try Cancel and the paste fallback.
    pub async fn sign_in(&self) -> Result<Account, BackendError> {
        tokio::time::sleep(Duration::from_secs(8)).await;
        Ok(Self::account())
    }

    /// Any pasted address with a `code=` signs in; anything else shows a
    /// typical Entra error.
    pub async fn finish_manual_sign_in(&self, pasted: &str) -> Result<Account, BackendError> {
        tokio::time::sleep(Duration::from_millis(600)).await;
        if pasted.contains("code=") {
            Ok(Self::account())
        } else {
            Err(BackendError::Auth(AuthError::OAuth(OAuthError::new(
                "invalid_request",
                "AADSTS900144: The request body must contain the following parameter: 'code'.",
            ))))
        }
    }

    pub fn sign_out(&self) {
        self.passwords.lock().unwrap().clear();
    }

    pub async fn fetch_feed(&self) -> Result<Feed, BackendError> {
        tokio::time::sleep(Duration::from_millis(900)).await;
        let attempt = {
            let mut fetches = self.fetches.lock().unwrap();
            *fetches += 1;
            *fetches
        };
        match self.feed_mode {
            // Fails until the user has pressed Retry twice, so both states can be seen.
            FeedMode::Fail if attempt < 3 => Err(BackendError::Feed(FeedError::Network(
                "error sending request for url (https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery)".into(),
            ))),
            FeedMode::Empty => Ok(Feed::default()),
            _ => Ok(feed()),
        }
    }

    pub fn saved_password(&self, id: &str) -> Option<String> {
        self.passwords.lock().unwrap().get(id).cloned()
    }

    pub fn remember_password(&self, id: &str, password: &str) {
        self.passwords.lock().unwrap().insert(id.to_owned(), password.to_owned());
    }

    pub fn forget_password(&self, id: &str) {
        self.passwords.lock().unwrap().remove(id);
    }

    /// Start a simulated connection. Must be called inside a Tokio runtime.
    pub fn connect(&self, id: &str, title: &str, password: Option<&str>, events: EventSink) -> Result<DemoSession, BackendError> {
        // Like the real backend: the client is checked after the password question.
        let needs_password = id == PASSWORD_ID && password.is_none_or(str::is_empty);
        if !needs_password {
            match self.freerdp {
                Some("missing") => return Err(BackendError::ClientMissing(ClientKind::Sdl)),
                Some(_) => {
                    return Err(BackendError::ClientTooOld { kind: ClientKind::Sdl, version: "3.10.2".into() });
                }
                None => {}
            }
        }
        if needs_password {
            return Err(BackendError::PasswordRequired);
        }
        let wrong_password = id == PASSWORD_ID && password != Some("demo");
        let script = if id == FAILING_ID { failing_script() } else if wrong_password { wrong_password_script() } else { normal_script() };

        let _ = std::fs::create_dir_all(&self.log_dir);
        let log_file = self.log_dir.join(format!("{}-{}.log", util::file_safe(title), util::random_token(3)));
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let file = log_file.clone();
        tokio::spawn(async move {
            let emit = |line: &str| {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
                    let _ = writeln!(f, "{line}");
                }
                events(SessionEvent::Log(line.to_owned()));
            };
            for step in script {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(step.delay_ms)) => {}
                    _ = &mut stop_rx => {
                        emit("[INFO][com.freerdp.client.sdl] - [sdl_run]: disconnect requested");
                        events(SessionEvent::Exited { code: None });
                        return;
                    }
                }
                match step.action {
                    Step::Log(line) => emit(line),
                    Step::Connected => {
                        emit("[INFO][com.freerdp.gdi] - [gdi_init_ex]: Local framebuffer format  PIXEL_FORMAT_BGRX32");
                        events(SessionEvent::Connected);
                    }
                    Step::Exit(code) => {
                        events(SessionEvent::Exited { code: Some(code) });
                        return;
                    }
                }
            }
        });
        Ok(DemoSession { stop: Some(stop_tx), log_file })
    }
}

/// A simulated session. Dropping it disconnects.
#[derive(Debug)]
pub struct DemoSession {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub log_file: PathBuf,
}

impl DemoSession {
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for DemoSession {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Step {
    Log(&'static str),
    Connected,
    Exit(i32),
}

struct Scripted {
    delay_ms: u64,
    action: Step,
}

fn at(delay_ms: u64, action: Step) -> Scripted {
    Scripted { delay_ms, action }
}

fn opening() -> Vec<Scripted> {
    vec![
        at(100, Step::Log("[INFO][com.freerdp.client.common.cmdline] - [freerdp_client_settings_parse_command_line]: demo session")),
        at(400, Step::Log("[INFO][com.freerdp.core.gateway.arm] - [arm_handle_request]: requesting the gateway token")),
        at(600, Step::Log("[INFO][com.freerdp.core.gateway.arm] - [arm_fill_gateway_parameters]: gateway rdgateway.example:443")),
    ]
}

fn normal_script() -> Vec<Scripted> {
    let mut steps = opening();
    steps.extend([
        at(700, Step::Log("[INFO][com.freerdp.core.aad] - [aad_send_auth_request]: session host sign-in")),
        at(500, Step::Connected),
        at(400, Step::Log("[INFO][com.freerdp.channels.rdpsnd.client] - [rdpsnd_recv_formats_pdu]: audio format PCM 48000 Hz")),
        at(20_000, Step::Log("[INFO][com.freerdp.core] - [rdp_recv_data_pdu]: the remote session was logged off")),
        at(200, Step::Exit(2)),
    ]);
    steps
}

fn failing_script() -> Vec<Scripted> {
    let mut steps = opening();
    steps.extend([
        at(900, Step::Log("[ERROR][com.freerdp.core.transport] - [transport_connect]: ERRCONNECT_CONNECT_TRANSPORT_FAILED [0x0002000D]")),
        at(200, Step::Exit(147)),
    ]);
    steps
}

fn wrong_password_script() -> Vec<Scripted> {
    let mut steps = opening();
    steps.extend([
        at(800, Step::Log("[ERROR][com.freerdp.core] - [nla_recv_pdu]: ERRCONNECT_LOGON_FAILURE [0x00020014]")),
        at(200, Step::Exit(154)),
    ]);
    steps
}

fn resource(id: &str, title: &str, kind: ResourceKind, workspace: &str) -> Resource {
    Resource {
        id: id.into(),
        title: title.into(),
        kind,
        workspace: workspace.into(),
        tenant_id: "demo".into(),
        feed_url: format!("https://rdweb.wvd.microsoft.com/api/arm/feeds/demo/{}", util::file_safe(workspace)),
        rdp_url: None,
        icon_url: None,
        folders: vec![],
    }
}

/// The demo feed: three workspaces with desktops and apps, and one warning.
pub fn feed() -> Feed {
    use ResourceKind::{Desktop, RemoteApp};
    Feed::from_resources(
        vec![
            resource("demo-cloud", "Cloud Desktop – Alex", Desktop, "Cloud Workstations"),
            resource("demo-finance", "Finance Desktop", Desktop, "Example Corp Production"),
            resource("demo-spreadsheet", "Spreadsheet", RemoteApp, "Example Corp Production"),
            resource("demo-erp", "ERP Client", RemoteApp, "Example Corp Production"),
            resource("demo-chat", "Team Chat", RemoteApp, "Example Corp Production"),
            resource(FAILING_ID, "Lab Desktop (fails)", Desktop, "Example Corp Lab"),
            resource(PASSWORD_ID, "Legacy Host (password: demo)", Desktop, "Example Corp Lab"),
            resource("demo-cad", "CAD Workstation", Desktop, "Example Corp Lab"),
        ],
        vec!["Partner Org: the service answered HTTP 503: Service Unavailable".into()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn demo_feed_has_distinct_keys_and_a_warning() {
        let feed = feed();
        assert_eq!(feed.workspaces.len(), 3);
        let mut keys: Vec<String> = feed.resources().map(|r| r.key()).collect();
        let total = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), total);
        assert_eq!(feed.warnings.len(), 1);
    }

    #[test]
    fn detects_the_flag() {
        assert!(requested(&["cloudrdpapp".into(), "--demo".into()]));
        assert!(!requested(&["cloudrdpapp".into()]) || std::env::var("CLOUDRDPAPP_DEMO").is_ok());
    }

    #[test]
    fn simulated_clients() {
        assert!(Demo::new(FeedMode::Normal, None).clients().sdl.unwrap().supports_helper());
        assert!(!Demo::new(FeedMode::Normal, Some("old")).clients().sdl.unwrap().supports_helper());
        assert_eq!(Demo::new(FeedMode::Normal, Some("missing")).clients(), Clients::default());
    }

    async fn run(demo: &Demo, id: &str, password: Option<&str>) -> Vec<SessionEvent> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let sink: EventSink = Arc::new(move |e| sink_seen.lock().unwrap().push(e));
        let session = demo.connect(id, "T", password, sink).unwrap();
        for _ in 0..100 {
            if seen.lock().unwrap().iter().any(|e| matches!(e, SessionEvent::Exited { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(session.log_file.is_file());
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn sessions_follow_their_scripts() {
        let demo = Demo::new(FeedMode::Normal, None);
        let events = run(&demo, FAILING_ID, None).await;
        assert!(!events.contains(&SessionEvent::Connected));
        assert_eq!(events.last(), Some(&SessionEvent::Exited { code: Some(147) }));

        assert!(matches!(
            demo.connect(PASSWORD_ID, "T", None, Arc::new(|_| {})),
            Err(BackendError::PasswordRequired)
        ));
        let events = run(&demo, PASSWORD_ID, Some("wrong")).await;
        assert_eq!(events.last(), Some(&SessionEvent::Exited { code: Some(154) }));

        // Stopping a normal session ends it without an exit code.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let mut session = demo.connect("x", "T", None, Arc::new(move |e| sink_seen.lock().unwrap().push(e))).unwrap();
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert!(seen.lock().unwrap().contains(&SessionEvent::Connected));
        session.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap().last(), Some(&SessionEvent::Exited { code: None }));
        let _ = std::fs::remove_dir_all(demo.log_dir());
    }

    #[tokio::test]
    async fn manual_sign_in_needs_a_code() {
        let demo = Demo::new(FeedMode::Normal, None);
        assert!(demo.finish_manual_sign_in("https://x/?code=1").await.is_ok());
        let err = demo.finish_manual_sign_in("nothing").await.unwrap_err();
        assert!(super::super::logic::sign_in_error_text(&err).contains("AADSTS900144"));
    }
}
