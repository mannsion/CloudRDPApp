//! The connection engine behind the GUI: account, feed, icons and FreeRDP
//! sessions, wired to the token relay and the helper bridge.
//!
//! The GUI only has to open URLs in the browser and show status text; both
//! are callbacks. Everything async runs on Tokio.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::auth::callback;
use crate::auth::manager::{LoginOptions, is_gateway_scope, is_session_host_scope};
use crate::auth::store::SecretStore;
use crate::auth::{Account, AuthError, AuthManager, OAuthClient, PendingLogin};
use crate::config::{ClientKind, ConnectAuth, ConnectionSettings};
use crate::consts;
use crate::feed::{Feed, FeedClient, FeedError, Resource};
use crate::helper::server::{HelperServer, NavigateHandler};
use crate::helper::{ERROR_CANCELLED, ERROR_TIMEOUT, NavigateRequest, NavigateResponse};
use crate::paths;
use crate::rdp::args::{self, LaunchInput, TokenRoute};
use crate::rdp::freerdp::{self, Client};
use crate::rdp::session::{self, EventSink, SessionControl, SessionEvent, SessionSpec};
use crate::rdp::{RdpFile, sdl_config};
use crate::relay::TokenRelay;
use crate::util;

/// Opens an address in the user's browser.
pub type OpenUrl = Arc<dyn Fn(&str) + Send + Sync>;
/// Shows a status line to the user.
pub type StatusSink = Arc<dyn Fn(String) + Send + Sync>;

/// Budget for an interactive browser sign-in started by the user.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Budget for a re-sign-in in the middle of a connection. FreeRDP gives its
/// helper 180 s per request; this leaves room to answer after the browser.
pub const RECONNECT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(170);

/// Everything the backend can fail with, in words for the user.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Sign-in failed: {0}.")]
    Auth(#[from] AuthError),
    #[error("The workspace service failed: {0}.")]
    Feed(#[from] FeedError),
    /// The host signs in with a password and none was given. Ask the user
    /// and call connect again with it.
    #[error("This desktop needs your password.")]
    PasswordRequired,
    #[error("{} is not installed. Install FreeRDP {} or later, or choose another client in Settings.", client_name(*.0), min_version())]
    ClientMissing(ClientKind),
    #[error("{} is version {version}, but FreeRDP {} or later is needed for single sign-on.", client_name(*.kind), min_version())]
    ClientTooOld { kind: ClientKind, version: String },
    #[error("The connection could not be started: {0}")]
    Launch(String),
}

fn client_name(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::Sdl => "The FreeRDP SDL client (sdl-freerdp3)",
        ClientKind::X11 => "The FreeRDP X11 client (xfreerdp3)",
    }
}

fn min_version() -> String {
    let (a, b, c) = consts::MIN_FREERDP_VERSION;
    format!("{a}.{b}.{c}")
}

/// The FreeRDP clients found on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clients {
    pub sdl: Option<Client>,
    pub x11: Option<Client>,
}

impl Clients {
    /// Look for both clients (blocking: runs `--version`).
    pub fn detect(sdl_path: &str, x11_path: &str) -> Self {
        Self {
            sdl: freerdp::detect(ClientKind::Sdl, sdl_path),
            x11: freerdp::detect(ClientKind::X11, x11_path),
        }
    }

    pub fn get(&self, kind: ClientKind) -> Option<&Client> {
        match kind {
            ClientKind::Sdl => self.sdl.as_ref(),
            ClientKind::X11 => self.x11.as_ref(),
        }
    }
}

/// Injected parts, for [`Backend::with_parts`].
pub struct BackendParts {
    pub auth: AuthManager,
    pub feed: FeedClient,
    /// Private directory for the relay bundle, helper socket and connection files.
    pub runtime_dir: PathBuf,
    /// Feed and icon cache.
    pub cache_dir: PathBuf,
    /// Session logs.
    pub log_dir: PathBuf,
    /// FreeRDP's SDL client configuration file (hotkey modifier).
    pub sdl_config: PathBuf,
    /// The executable FreeRDP starts as its sign-in helper.
    pub helper_exe: PathBuf,
    /// Executable to register as the `ms-appx-web:` handler before a browser
    /// sign-in (`crate::url_handler`); `None` leaves the desktop alone (tests).
    pub url_handler_exe: Option<PathBuf>,
    pub clients: Clients,
    /// Extra environment for every FreeRDP child (tests use this).
    pub child_env: Vec<(String, String)>,
    pub open_url: OpenUrl,
    pub status: StatusSink,
}

/// The engine. Cheap to clone.
#[derive(Clone)]
pub struct Backend {
    inner: Arc<Inner>,
}

struct Inner {
    auth: AuthManager,
    feed: FeedClient,
    relay: TokenRelay,
    helper: HelperServer,
    clients: Mutex<Clients>,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
    log_dir: PathBuf,
    sdl_config: PathBuf,
    helper_exe: PathBuf,
    url_handler_exe: Option<PathBuf>,
    child_env: Vec<(String, String)>,
    open_url: OpenUrl,
    status: StatusSink,
    /// One interactive re-sign-in at a time, however many token requests wait for it.
    reauth: tokio::sync::Mutex<()>,
}

/// A running FreeRDP session. Dropping it disconnects.
#[derive(Debug)]
pub struct Session {
    control: SessionControl,
    pub title: String,
    pub client: ClientKind,
}

impl Session {
    /// Ask FreeRDP to disconnect. Safe to call more than once.
    pub fn stop(&mut self) {
        self.control.stop();
    }

    /// FreeRDP's log for this session.
    pub fn log_file(&self) -> &Path {
        &self.control.log_file
    }
}

impl Backend {
    /// Production start-up: keyring, relay, helper socket and client
    /// detection. `sdl_path`/`x11_path` are the configured client paths
    /// (empty = search `PATH`).
    pub async fn start(open_url: OpenUrl, status: StatusSink, sdl_path: &str, x11_path: &str) -> Result<Self, String> {
        let http = crate::http_client();
        let auth = AuthManager::new(OAuthClient::new(http.clone()), SecretStore::open().await);
        let (sdl, x11) = (sdl_path.to_owned(), x11_path.to_owned());
        let clients = tokio::task::spawn_blocking(move || Clients::detect(&sdl, &x11))
            .await
            .map_err(|err| format!("looking for FreeRDP failed: {err}"))?;
        let helper_exe = std::env::current_exe().map_err(|err| format!("cannot locate this program: {err}"))?;
        session::prune_logs(30);
        Self::with_parts(BackendParts {
            auth,
            feed: FeedClient::new(http),
            runtime_dir: paths::runtime_dir(),
            cache_dir: paths::cache_dir(),
            log_dir: paths::logs_dir(),
            sdl_config: paths::freerdp_sdl_config(),
            url_handler_exe: Some(helper_exe.clone()),
            helper_exe,
            clients,
            child_env: Vec::new(),
            open_url,
            status,
        })
        .await
    }

    /// Start from injected parts (tests, demo mode).
    pub async fn with_parts(parts: BackendParts) -> Result<Self, String> {
        let relay = TokenRelay::start(parts.auth.clone(), &parts.runtime_dir).await?;
        let helper = HelperServer::start(&parts.runtime_dir, parts.auth.callbacks().clone()).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                auth: parts.auth,
                feed: parts.feed,
                relay,
                helper,
                clients: Mutex::new(parts.clients),
                runtime_dir: parts.runtime_dir,
                cache_dir: parts.cache_dir,
                log_dir: parts.log_dir,
                sdl_config: parts.sdl_config,
                helper_exe: parts.helper_exe,
                url_handler_exe: parts.url_handler_exe,
                child_env: parts.child_env,
                open_url: parts.open_url,
                status: parts.status,
                reauth: tokio::sync::Mutex::new(()),
            }),
        })
    }

    pub fn auth(&self) -> &AuthManager {
        &self.inner.auth
    }

    pub fn relay(&self) -> &TokenRelay {
        &self.inner.relay
    }

    pub fn clients(&self) -> Clients {
        self.inner.clients.lock().unwrap().clone()
    }

    /// Look for the clients again (after the paths changed in Settings).
    pub async fn redetect_clients(&self, sdl_path: &str, x11_path: &str) -> Clients {
        let (sdl, x11) = (sdl_path.to_owned(), x11_path.to_owned());
        let found = tokio::task::spawn_blocking(move || Clients::detect(&sdl, &x11)).await.unwrap_or_default();
        *self.inner.clients.lock().unwrap() = found.clone();
        found
    }

    // --- account -------------------------------------------------------------

    pub async fn account(&self) -> Option<Account> {
        self.inner.auth.account().await
    }

    /// Load the saved sign-in of `account`. False when there is none.
    pub async fn restore_account(&self, account: &Account) -> bool {
        self.inner.auth.restore(account).await
    }

    /// Interactive sign-in in the browser, returning through the broker
    /// redirect and `--auth-callback`. Cancel by dropping the future.
    pub async fn sign_in(&self) -> Result<Account, BackendError> {
        ensure_url_handler(&self.inner).await;
        let options = LoginOptions { prompt: Some("select_account".to_owned()), ..Default::default() };
        let pending = self.inner.auth.start_login(&options);
        (self.inner.open_url)(&pending.url);
        Ok(self.inner.auth.finish_login(pending, SIGN_IN_TIMEOUT, None).await?)
    }

    /// Fallback sign-in: open the returned `url`, then pass what the user
    /// pastes to [`finish_manual_sign_in`](Self::finish_manual_sign_in).
    pub fn start_manual_sign_in(&self) -> PendingLogin {
        let pending = self
            .inner
            .auth
            .start_manual_login(&LoginOptions { prompt: Some("select_account".to_owned()), ..Default::default() });
        (self.inner.open_url)(&pending.url);
        pending
    }

    pub async fn finish_manual_sign_in(&self, pending: &PendingLogin, pasted: &str) -> Result<Account, BackendError> {
        Ok(self.inner.auth.finish_manual_login(pending, pasted, None).await?)
    }

    /// Forget the account, its saved sign-in and its cached feed.
    pub async fn sign_out(&self) {
        if let Some(account) = self.inner.auth.account().await {
            let _ = std::fs::remove_file(feed_cache_file(&self.inner.cache_dir, &account));
        }
        self.inner.auth.sign_out().await;
    }

    // --- feed ----------------------------------------------------------------

    /// Run a feed request with a fresh token; on 401 refresh once and retry.
    async fn with_token<T, F, Fut>(&self, request: F) -> Result<T, BackendError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, FeedError>>,
    {
        let token = self.inner.auth.access_token(consts::WVD_SCOPE).await?;
        match request(token).await {
            Err(FeedError::Unauthorized) => {
                self.inner.auth.invalidate(consts::WVD_SCOPE).await;
                let token = self.inner.auth.access_token(consts::WVD_SCOPE).await?;
                Ok(request(token).await?)
            }
            other => Ok(other?),
        }
    }

    /// Download the feed and remember it for the next start-up.
    pub async fn fetch_feed(&self) -> Result<Feed, BackendError> {
        let feed_client = self.inner.feed.clone();
        let feed = self
            .with_token(|token| {
                let client = feed_client.clone();
                async move { client.fetch(&token).await }
            })
            .await?;
        if let Some(account) = self.inner.auth.account().await
            && let Err(err) = save_feed_cache(&self.inner.cache_dir, &account, &feed)
        {
            tracing::warn!("could not cache the feed: {err}");
        }
        Ok(feed)
    }

    /// The last feed downloaded for `account`, for an instant first screen.
    pub fn cached_feed(&self, account: &Account) -> Option<Feed> {
        load_feed_cache(&self.inner.cache_dir, account)
    }

    /// Path of the resource's icon, downloading it if needed. `None` when
    /// it has none or it cannot be fetched (logged, never fatal).
    pub async fn icon(&self, resource: &Resource) -> Option<PathBuf> {
        let url = resource.icon_url.clone()?;
        let path = icon_path(&self.inner.cache_dir, resource, &url);
        if path.is_file() {
            return Some(path);
        }
        let feed_client = self.inner.feed.clone();
        let bytes = match self
            .with_token(|token| {
                let client = feed_client.clone();
                let url = url.clone();
                async move { client.download_icon(&token, &url).await }
            })
            .await
        {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => return None,
            Err(err) => {
                tracing::debug!("icon for {} unavailable: {err}", resource.title);
                return None;
            }
        };
        match paths::write_private_file(&path, &bytes) {
            Ok(()) => Some(path),
            Err(err) => {
                tracing::debug!("could not cache the icon for {}: {err}", resource.title);
                None
            }
        }
    }

    // --- connect -------------------------------------------------------------

    /// Download the resource's connection file and start FreeRDP.
    ///
    /// Returns [`BackendError::PasswordRequired`] when the host signs in
    /// with a password and `password` is `None`; ask and call again.
    pub async fn connect(
        &self,
        resource: &Resource,
        settings: &ConnectionSettings,
        password: Option<&str>,
        local_scale_percent: Option<u32>,
        events: EventSink,
    ) -> Result<Session, BackendError> {
        let feed_client = self.inner.feed.clone();
        let text = self
            .with_token(|token| {
                let client = feed_client.clone();
                let resource = resource.clone();
                async move { client.download_rdp(&token, &resource).await }
            })
            .await?;
        self.connect_rdp(&text, &resource.title, settings, password, local_scale_percent, events).await
    }

    /// Start FreeRDP for an already downloaded `.rdp` text ("open .rdp file").
    pub async fn connect_rdp(
        &self,
        rdp_text: &str,
        title: &str,
        settings: &ConnectionSettings,
        password: Option<&str>,
        local_scale_percent: Option<u32>,
        events: EventSink,
    ) -> Result<Session, BackendError> {
        let rdp = RdpFile::parse(rdp_text);
        if rdp.property_count() == 0 {
            return Err(BackendError::Launch("the connection file is empty or not an .rdp file.".to_owned()));
        }
        let password = password.filter(|p| !p.is_empty());
        if !args::uses_entra_sso(settings, &rdp) && password.is_none() {
            return Err(BackendError::PasswordRequired);
        }

        let client = self.pick_client(settings, &rdp)?;
        let username = self.inner.auth.account().await.map(|a| a.username).unwrap_or_default();

        let token_route = match settings.advanced.connect_auth {
            ConnectAuth::Silent => TokenRoute::Relay { authority: self.inner.relay.authority() },
            ConnectAuth::Browser => TokenRoute::Browser,
        };
        let relay_route = matches!(token_route, TokenRoute::Relay { .. });

        let plan = args::build(LaunchInput {
            settings,
            rdp,
            client: client.kind,
            program: client.path.clone(),
            username: &username,
            password,
            title,
            helper_exe: &self.inner.helper_exe,
            token_route,
            local_scale_percent,
        })
        .map_err(|err| BackendError::Launch(sentence(&err)))?;

        if client.kind == ClientKind::Sdl
            && let Err(err) = sdl_config::apply(settings.advanced.hotkeys, &self.inner.sdl_config)
        {
            tracing::warn!("could not set the SDL hotkey modifier: {err}");
        }

        let handler = if relay_route {
            relay_handler(Arc::downgrade(&self.inner), title.to_owned())
        } else {
            browser_handler(Arc::downgrade(&self.inner))
        };
        let registration = self.inner.helper.register(handler);

        let mut env: Vec<(String, String)> = Vec::new();
        if relay_route {
            env.push(("SSL_CERT_FILE".into(), self.inner.relay.ca_bundle().to_string_lossy().into_owned()));
        }
        env.push((consts::ENV_HELPER_SOCKET.into(), self.inner.helper.socket_path().to_string_lossy().into_owned()));
        env.push((consts::ENV_HELPER_SESSION.into(), registration.session.clone()));
        env.push((consts::ENV_HELPER_SECRET.into(), registration.secret.clone()));
        env.extend(self.inner.child_env.iter().cloned());

        let spec = SessionSpec {
            plan,
            dir: self.inner.runtime_dir.join("sessions").join(&registration.session),
            log_file: session::log_file_in(&self.inner.log_dir, title),
            env,
        };

        let helper = self.inner.helper.clone();
        let session_id = registration.session.clone();
        let sink: EventSink = Arc::new(move |event: SessionEvent| {
            if matches!(event, SessionEvent::Exited { .. }) {
                helper.unregister(&session_id);
            }
            events(event);
        });
        match session::spawn(spec, sink) {
            Ok(control) => Ok(Session { control, title: title.to_owned(), client: client.kind }),
            Err(err) => {
                self.inner.helper.unregister(&registration.session);
                Err(BackendError::Launch(sentence(&err)))
            }
        }
    }

    fn pick_client(&self, settings: &ConnectionSettings, rdp: &RdpFile) -> Result<Client, BackendError> {
        let clients = self.inner.clients.lock().unwrap();
        let kind = args::choose_client(settings, rdp.is_remote_app(), clients.x11.is_some());
        check_client(clients.get(kind), kind)
    }
}

/// Refuse a missing client or one without the helper protocol.
fn check_client(client: Option<&Client>, kind: ClientKind) -> Result<Client, BackendError> {
    let client = client.ok_or(BackendError::ClientMissing(kind))?;
    if !client.supports_helper() {
        return Err(BackendError::ClientTooOld { kind, version: client.version_string() });
    }
    Ok(client.clone())
}

fn sentence(text: &str) -> String {
    let mut out = text.trim().to_owned();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

// --- navigate handlers ---------------------------------------------------------

/// `scope` and `state` from the authorize URL FreeRDP wants opened.
pub fn parse_authorize_request(url: &str) -> Result<(String, Option<String>), String> {
    let parsed = url::Url::parse(url).map_err(|err| format!("FreeRDP sent an unreadable sign-in address: {err}"))?;
    let mut scope = None;
    let mut state = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "scope" if scope.is_none() => scope = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            _ => {}
        }
    }
    let scope = scope.filter(|s| !s.trim().is_empty()).ok_or("FreeRDP's sign-in request names no scope")?;
    Ok((scope, state))
}

/// The address FreeRDP expects back: its redirect URI with our code and its state.
pub fn relay_redirect_url(redirect_uri: &str, code: &str, state: Option<&str>) -> String {
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut out = format!("{redirect_uri}{separator}code={}", util::percent_encode(code));
    if let Some(state) = state {
        out.push_str("&state=");
        out.push_str(&util::percent_encode(state));
    }
    out
}

fn auth_error_response(err: &AuthError) -> NavigateResponse {
    match err {
        AuthError::Timeout => NavigateResponse::error(ERROR_TIMEOUT),
        AuthError::Cancelled => NavigateResponse::error(ERROR_CANCELLED),
        other => NavigateResponse::error(other.to_string()),
    }
}

fn relay_handler(inner: Weak<Inner>, title: String) -> NavigateHandler {
    Arc::new(move |request: NavigateRequest| {
        let inner = inner.clone();
        let title = title.clone();
        Box::pin(async move {
            match inner.upgrade() {
                Some(inner) => relay_navigate(&inner, &title, request).await,
                None => NavigateResponse::error(format!("{} is shutting down", consts::APP_NAME)),
            }
        })
    })
}

async fn relay_navigate(inner: &Inner, title: &str, request: NavigateRequest) -> NavigateResponse {
    let (scope, state) = match parse_authorize_request(&request.url) {
        Ok(parsed) => parsed,
        Err(err) => return NavigateResponse::error(err),
    };
    if !is_gateway_scope(&scope) && !is_session_host_scope(&scope) {
        tracing::warn!("refusing a token for an unexpected scope: {scope}");
        return NavigateResponse::error(format!("{} does not issue tokens for {scope}", consts::APP_NAME));
    }
    if let Err(err) = ensure_silent_token(inner, &scope).await {
        if !err.needs_interaction() {
            return auth_error_response(&err);
        }
        if let Err(err) = reauthenticate(inner, title, &scope, &err).await {
            return auth_error_response(&err);
        }
        if let Err(err) = ensure_silent_token(inner, &scope).await {
            return auth_error_response(&err);
        }
    }
    let code = inner.relay.issue_code(&scope);
    NavigateResponse::ok(relay_redirect_url(&request.redirect_uri, &code, state.as_deref()))
}

/// Make sure the relay will be able to mint a token for `scope`.
async fn ensure_silent_token(inner: &Inner, scope: &str) -> Result<(), AuthError> {
    if is_gateway_scope(scope) {
        inner.auth.access_token(scope).await.map(|_| ())
    } else {
        inner.auth.preflight(scope).await
    }
}

async fn reauthenticate(inner: &Inner, title: &str, scope: &str, cause: &AuthError) -> Result<(), AuthError> {
    let _one_at_a_time = inner.reauth.lock().await;
    if ensure_silent_token(inner, scope).await.is_ok() {
        return Ok(());
    }
    let expected = inner.auth.account().await;
    let options = LoginOptions {
        login_hint: expected.as_ref().map(|a| a.username.clone()),
        claims: cause.claims().map(str::to_owned),
        prompt: None,
    };
    ensure_url_handler(inner).await;
    let pending = inner.auth.start_login(&options);
    (inner.status)(format!("Sign in again in your browser to continue connecting to {title}."));
    (inner.open_url)(&pending.url);
    let result = inner.auth.finish_login(pending, RECONNECT_SIGN_IN_TIMEOUT, expected.as_ref()).await;
    match &result {
        Ok(_) => (inner.status)(format!("Signed in. Connecting to {title}…")),
        Err(err) => (inner.status)(format!("Sign-in for {title} did not complete: {err}.")),
    }
    result.map(|_| ())
}

/// Make this program the `ms-appx-web:` handler if it is not already.
/// Failure is not fatal: the paste fallback still works.
async fn ensure_url_handler(inner: &Inner) {
    let Some(exe) = inner.url_handler_exe.clone() else { return };
    let result = tokio::task::spawn_blocking(move || crate::url_handler::ensure_registered(&exe))
        .await
        .unwrap_or_else(|err| Err(err.to_string()));
    match result {
        Ok(crate::url_handler::Outcome::Registered { replaced }) => {
            tracing::info!("registered for {} (replaced: {replaced:?})", crate::url_handler::MIME_TYPE);
        }
        Ok(crate::url_handler::Outcome::AlreadyRegistered) => {}
        Err(err) => {
            tracing::warn!("could not register the sign-in handler: {err}");
            (inner.status)(format!(
                "{} could not register itself to receive the browser sign-in ({err}). If the browser cannot return, use “Paste the address instead”.",
                consts::APP_NAME
            ));
        }
    }
}

/// FreeRDP's own browser sign-in (connection-time fallback route). FreeRDP
/// keeps its default broker redirect, so the browser returns through
/// `--auth-callback` like the app's own sign-in; the callback is matched by
/// the `state` FreeRDP put in its URL and handed back unchanged.
fn browser_handler(inner: Weak<Inner>) -> NavigateHandler {
    Arc::new(move |request: NavigateRequest| {
        let inner = inner.clone();
        Box::pin(async move {
            let Some(inner) = inner.upgrade() else {
                return NavigateResponse::error(format!("{} is shutting down", consts::APP_NAME));
            };
            browser_navigate(&inner, request).await
        })
    })
}

async fn browser_navigate(inner: &Inner, request: NavigateRequest) -> NavigateResponse {
    let state = match parse_authorize_request(&request.url) {
        Ok((_, Some(state))) if !state.is_empty() => state,
        Ok(_) => return NavigateResponse::error("FreeRDP's sign-in request carries no state, so its response could not be recognised"),
        Err(err) => return NavigateResponse::error(err),
    };
    if !callback::is_broker_redirect(&request.redirect_uri) {
        tracing::warn!(
            "FreeRDP asked for redirect {}; only the broker redirect can return to {}",
            request.redirect_uri,
            consts::APP_NAME
        );
    }
    ensure_url_handler(inner).await;
    let waiter = inner.auth.callbacks().expect(&state);
    (inner.status)(format!(
        "Continue signing in in your browser. When it asks to open {}, allow it.",
        consts::APP_NAME
    ));
    (inner.open_url)(&request.url);
    // Answer a little before FreeRDP's own deadline.
    let budget = Duration::from_millis(request.timeout_ms.max(10_000)).saturating_sub(Duration::from_secs(5));
    match waiter.wait(budget).await {
        Ok(landed) => NavigateResponse::ok(landed),
        Err(err) => auth_error_response(&err),
    }
}

// --- caches ----------------------------------------------------------------------

fn feed_cache_file(cache_dir: &Path, account: &Account) -> PathBuf {
    cache_dir.join("feeds").join(format!("{}.json", util::short_hash(&account.id())))
}

fn save_feed_cache(cache_dir: &Path, account: &Account, feed: &Feed) -> Result<(), String> {
    let json = serde_json::to_vec(feed).map_err(|err| err.to_string())?;
    let path = feed_cache_file(cache_dir, account);
    if let Some(parent) = path.parent() {
        paths::create_private_dir(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    paths::write_private_file(&path, &json).map_err(|err| format!("{}: {err}", path.display()))
}

fn load_feed_cache(cache_dir: &Path, account: &Account) -> Option<Feed> {
    let bytes = std::fs::read(feed_cache_file(cache_dir, account)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn icon_path(cache_dir: &Path, resource: &Resource, url: &str) -> PathBuf {
    let ext = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            Path::new(u.path())
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
        })
        .filter(|e| matches!(e.as_str(), "png" | "ico" | "jpg" | "jpeg" | "svg" | "bmp" | "gif"))
        .unwrap_or_else(|| "png".to_owned());
    cache_dir.join("icons").join(format!("{}.{ext}", resource.key()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostAuth;
    use crate::feed::ResourceKind;

    const RDP: &str = "full address:s:host\r\nresourceprovider:s:arm\r\nenablerdsaadauth:i:1\r\nloadbalanceinfo:s:x\r\nremoteapplicationprogram:s:\r\n";

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cloudrdpapp-backend-{}", util::random_token(6)))
    }

    async fn backend(root: &Path, clients: Clients) -> Backend {
        backend_with(root, clients, Arc::new(|_| {}), Arc::new(|_| {})).await
    }

    async fn backend_with(root: &Path, clients: Clients, open_url: OpenUrl, status: StatusSink) -> Backend {
        let auth = AuthManager::new(OAuthClient::with_authority(crate::http_client(), "http://127.0.0.1:9"), SecretStore::in_memory());
        Backend::with_parts(BackendParts {
            auth,
            feed: FeedClient::for_testing(crate::http_client(), "http://127.0.0.1:9/discovery"),
            runtime_dir: root.join("run"),
            cache_dir: root.join("cache"),
            log_dir: root.join("logs"),
            sdl_config: root.join("sdl-freerdp.json"),
            helper_exe: "/usr/bin/true".into(),
            url_handler_exe: None,
            clients,
            child_env: vec![],
            open_url,
            status,
        })
        .await
        .unwrap()
    }

    fn sink() -> EventSink {
        Arc::new(|_| {})
    }

    fn client(kind: ClientKind, version: Option<(u32, u32, u32)>) -> Client {
        Client { kind, path: "/nonexistent/freerdp".into(), version }
    }

    fn resource(icon: Option<&str>) -> Resource {
        Resource {
            id: "r1".into(),
            title: "Desk".into(),
            kind: ResourceKind::Desktop,
            workspace: "W".into(),
            tenant_id: "t".into(),
            feed_url: "https://rdweb.wvd.microsoft.com/f".into(),
            rdp_url: None,
            icon_url: icon.map(str::to_owned),
            folders: vec![],
        }
    }

    #[test]
    fn parses_freerdp_authorize_urls() {
        let url = "https://127.0.0.1:4000/common/oauth2/v2.0/authorize?client_id=a85cf173&response_type=code\
            &scope=https%3A%2F%2Fwww.wvd.microsoft.com%2F.default%20openid%20profile%20offline_access\
            &redirect_uri=ms-appx-web%3a%2f%2fMicrosoft.AAD.BrokerPlugin%2fa85cf173&login_hint=a%40b.c\
            &state=0a1b2c&code_challenge=xyz&code_challenge_method=S256";
        let (scope, state) = parse_authorize_request(url).unwrap();
        assert_eq!(scope, "https://www.wvd.microsoft.com/.default openid profile offline_access");
        assert!(is_gateway_scope(&scope));
        assert_eq!(state.as_deref(), Some("0a1b2c"));

        let host = "https://127.0.0.1:1/common/oauth2/v2.0/authorize?scope=ms-device-service%3A%2F%2Ftermsrv.wvd.microsoft.com%2Fname%2Fhost%2Fuser_impersonation";
        let (scope, state) = parse_authorize_request(host).unwrap();
        assert!(is_session_host_scope(&scope));
        assert_eq!(state, None);

        assert!(parse_authorize_request("https://x/authorize?state=1").is_err());
        assert!(parse_authorize_request("not a url").is_err());
    }

    #[test]
    fn builds_the_redirect_freerdp_expects() {
        assert_eq!(
            relay_redirect_url("ms-appx-web://Microsoft.AAD.BrokerPlugin/a85cf173", "c0de-_x", Some("abc123")),
            "ms-appx-web://Microsoft.AAD.BrokerPlugin/a85cf173?code=c0de-_x&state=abc123"
        );
        assert_eq!(relay_redirect_url("https://x/cb?k=v", "c", None), "https://x/cb?k=v&code=c");
    }

    #[test]
    fn client_checks() {
        assert!(matches!(check_client(None, ClientKind::Sdl), Err(BackendError::ClientMissing(ClientKind::Sdl))));
        let old = client(ClientKind::X11, Some((3, 10, 0)));
        let err = check_client(Some(&old), ClientKind::X11).unwrap_err();
        assert!(matches!(&err, BackendError::ClientTooOld { version, .. } if version == "3.10.0"));
        assert!(err.to_string().contains("3.32.0 or later"), "{err}");
        let unknown = client(ClientKind::Sdl, None);
        assert!(matches!(check_client(Some(&unknown), ClientKind::Sdl), Err(BackendError::ClientTooOld { .. })));
        let good = client(ClientKind::Sdl, Some((3, 33, 1)));
        assert_eq!(check_client(Some(&good), ClientKind::Sdl).unwrap(), good);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_refuses_missing_and_old_clients() {
        let root = test_dir();
        let backend = backend(&root, Clients::default()).await;
        let settings = ConnectionSettings::default();
        let err = backend.connect_rdp(RDP, "Desk", &settings, None, None, sink()).await.unwrap_err();
        assert!(matches!(err, BackendError::ClientMissing(ClientKind::Sdl)), "{err}");

        let backend = self::backend(&root, Clients { sdl: Some(client(ClientKind::Sdl, Some((3, 5, 0)))), x11: None }).await;
        let err = backend.connect_rdp(RDP, "Desk", &settings, None, None, sink()).await.unwrap_err();
        assert!(matches!(err, BackendError::ClientTooOld { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn password_hosts_ask_for_a_password() {
        let root = test_dir();
        let backend = backend(&root, Clients::default()).await;
        let mut settings = ConnectionSettings::default();
        settings.advanced.host_auth = HostAuth::Password;
        // Checked before the client, so the GUI can ask first.
        let err = backend.connect_rdp(RDP, "Desk", &settings, None, None, sink()).await.unwrap_err();
        assert!(matches!(err, BackendError::PasswordRequired));
        let err = backend.connect_rdp(RDP, "Desk", &settings, Some(""), None, sink()).await.unwrap_err();
        assert!(matches!(err, BackendError::PasswordRequired));
        // With a password the next check (the client) is reached.
        let err = backend.connect_rdp(RDP, "Desk", &settings, Some("pw"), None, sink()).await.unwrap_err();
        assert!(matches!(err, BackendError::ClientMissing(_)));
        // An SSO host needs none.
        let err = backend
            .connect_rdp(RDP, "Desk", &ConnectionSettings::default(), None, None, sink())
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::ClientMissing(_)));
        assert!(matches!(
            backend.connect_rdp("garbage", "x", &settings, None, None, sink()).await,
            Err(BackendError::Launch(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_failures_are_reported() {
        let root = test_dir();
        let backend = backend(&root, Clients { sdl: Some(client(ClientKind::Sdl, Some((3, 33, 1)))), x11: None }).await;
        let mut settings = ConnectionSettings::default();
        settings.advanced.hotkeys = crate::config::HotkeyModifier::Unmanaged;
        let err = backend.connect_rdp(RDP, "Desk", &settings, None, None, sink()).await.unwrap_err();
        assert!(matches!(&err, BackendError::Launch(text) if text.contains("could not start")), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_handler_refuses_foreign_scopes() {
        let root = test_dir();
        let backend = backend(&root, Clients::default()).await;
        let request = |url: &str| NavigateRequest {
            session: "s".into(),
            secret: "x".into(),
            url: url.into(),
            redirect_uri: "ms-appx-web://Microsoft.AAD.BrokerPlugin/c".into(),
            title: String::new(),
            timeout_ms: 1000,
        };
        let answer = relay_navigate(&backend.inner, "Desk", request("https://h/authorize?scope=https%3A%2F%2Fgraph.microsoft.com%2F.default&state=1")).await;
        assert!(answer.error.unwrap().contains("does not issue"));
        let answer = relay_navigate(&backend.inner, "Desk", request("https://h/authorize?state=1")).await;
        assert!(answer.error.unwrap().contains("no scope"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_handler_signs_in_again_when_needed() {
        let root = test_dir();
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let statuses = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = opened.clone();
        let hub: Arc<Mutex<Option<callback::CallbackHub>>> = Arc::default();
        let hub_slot = hub.clone();
        // The "browser": the user declines, so Entra redirects back with an
        // error, which the desktop hands to `--auth-callback`.
        let open_url: OpenUrl = Arc::new(move |url: &str| {
            seen.lock().unwrap().push(url.to_owned());
            let parsed = url::Url::parse(url).unwrap();
            let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
            let back = format!(
                "{}?error=access_denied&error_description=declined&state={}",
                pairs["redirect_uri"], pairs["state"]
            );
            hub_slot.lock().unwrap().as_ref().unwrap().deliver(&back).unwrap();
        });
        let sink = statuses.clone();
        let status: StatusSink = Arc::new(move |text| sink.lock().unwrap().push(text));
        let backend = backend_with(&root, Clients::default(), open_url, status).await;
        *hub.lock().unwrap() = Some(backend.auth().callbacks().clone());
        let request = NavigateRequest {
            session: "s".into(),
            secret: "x".into(),
            url: "https://h/authorize?scope=https%3A%2F%2Fwww.wvd.microsoft.com%2F.default%20openid&state=1".into(),
            redirect_uri: "ms-appx-web://Microsoft.AAD.BrokerPlugin/c".into(),
            title: String::new(),
            timeout_ms: 1000,
        };
        // Not signed in: the handler starts a browser sign-in, which fails.
        let answer = relay_navigate(&backend.inner, "Desk", request).await;
        assert!(answer.redirect_url.is_none());
        assert!(answer.error.is_some());
        let opened = opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].starts_with("http://127.0.0.1:9/organizations/oauth2/v2.0/authorize?"));
        assert!(opened[0].contains("redirect_uri=ms-appx-web%3A%2F%2FMicrosoft.AAD.BrokerPlugin%2Fa85cf173-4192-42f8-81fa-777a763e6e2c&"));
        assert_eq!(answer.error.as_deref(), Some("declined"), "{answer:?}");
        let statuses = statuses.lock().unwrap();
        assert!(statuses[0].contains("Sign in again") && statuses[0].contains("Desk"), "{statuses:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_route_returns_the_callback_matching_freerdps_state() {
        let root = test_dir();
        let hub: Arc<Mutex<Option<callback::CallbackHub>>> = Arc::default();
        let hub_slot = hub.clone();
        let open_url: OpenUrl = Arc::new(move |url: &str| {
            let hub = hub_slot.lock().unwrap().clone().unwrap();
            let state = parse_authorize_request(url).unwrap().1.unwrap();
            // A stray response for someone else's sign-in first; it is refused.
            assert!(hub.deliver("ms-appx-web://Microsoft.AAD.BrokerPlugin/c?code=nope&state=other").is_err());
            hub.deliver(&format!("MS-APPX-WEB://microsoft.aad.brokerplugin/c?code=the-code&state={state}")).unwrap();
        });
        let backend = backend_with(&root, Clients::default(), open_url, Arc::new(|_| {})).await;
        *hub.lock().unwrap() = Some(backend.auth().callbacks().clone());
        let request = |url: &str| NavigateRequest {
            session: "s".into(),
            secret: "x".into(),
            url: url.into(),
            redirect_uri: "ms-appx-web://Microsoft.AAD.BrokerPlugin/c".into(),
            title: String::new(),
            timeout_ms: 20_000,
        };
        let answer = browser_navigate(
            &backend.inner,
            request("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id=c&scope=s&redirect_uri=ms-appx-web%3a%2f%2fMicrosoft.AAD.BrokerPlugin%2fc&state=f00d&code_challenge=x"),
        )
        .await;
        assert_eq!(answer.redirect_url.as_deref(), Some("MS-APPX-WEB://microsoft.aad.brokerplugin/c?code=the-code&state=f00d"));
        assert_eq!(backend.auth().callbacks().pending(), 0);

        let answer = browser_navigate(&backend.inner, request("https://h/authorize?scope=s")).await;
        assert!(answer.error.unwrap().contains("no state"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feed_cache_round_trip() {
        let root = test_dir();
        let backend = backend(&root, Clients::default()).await;
        let account = Account { tenant_id: "t".into(), object_id: "o".into(), username: "u@x.io".into(), name: None };
        assert_eq!(backend.cached_feed(&account), None);
        let feed = Feed::from_resources(vec![resource(Some("https://x/icon.PNG"))], vec!["warning".into()]);
        save_feed_cache(&root.join("cache"), &account, &feed).unwrap();
        assert_eq!(backend.cached_feed(&account), Some(feed));
        use std::os::unix::fs::PermissionsExt;
        let file = feed_cache_file(&root.join("cache"), &account);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);

        // Signing out removes it.
        backend.auth().set_session(account.clone(), "rt").await;
        backend.sign_out().await;
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn icons_are_cached_by_resource_key() {
        let root = test_dir();
        let backend = backend(&root, Clients::default()).await;
        let r = resource(Some("https://rdweb.wvd.microsoft.com/api/icon/x.ICO?fmt=1"));
        let path = icon_path(&root.join("cache"), &r, r.icon_url.as_deref().unwrap());
        assert_eq!(path, root.join("cache").join("icons").join(format!("{}.ico", r.key())));
        assert_eq!(
            icon_path(&root.join("cache"), &r, "https://x/icon?format=png").extension().unwrap(),
            "png"
        );
        assert_eq!(backend.icon(&resource(None)).await, None);
        // Not signed in: no download, no error.
        assert_eq!(backend.icon(&r).await, None);
        // Already cached: returned without any request.
        paths::write_private_file(&path, b"png").unwrap();
        assert_eq!(backend.icon(&r).await, Some(path));
        let _ = std::fs::remove_dir_all(&root);
    }
}
