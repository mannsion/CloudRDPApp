//! The silent connection sign-in, end to end, against the installed FreeRDP.
//!
//! Everything is local: a mock Entra token endpoint on plain HTTP, the app's
//! own token relay and helper bridge, and a fake AVD gateway on TLS that
//! answers 401 after recording what FreeRDP sent. Nothing is shown on the
//! desktop (SDL's dummy video driver, connection dialog off).
//!
//! Proves with the real binary: `/args-from:fd:3`, `/azure:ad:host:port`,
//! trust of the relay via `SSL_CERT_FILE`, the helper bridge and its socket,
//! the one-time code and redirect shape, and the relay's token response.
//! Does not reach the session-host (`req_cnf`) leg: that needs a real host.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

use cloudrdpapp::auth::store::SecretStore;
use cloudrdpapp::auth::{Account, AuthManager, OAuthClient};
use cloudrdpapp::backend::{Backend, BackendParts, Clients};
use cloudrdpapp::config::{
    AudioPlayback, CertPolicy, ClientKind, ConnectionSettings, DisplayMode, HotkeyModifier, LogLevel,
};
use cloudrdpapp::feed::FeedClient;
use cloudrdpapp::rdp::freerdp;
use cloudrdpapp::rdp::session::{EventSink, SessionEvent};

const FAKE_TOKEN: &str = "fake-gateway-token-7f3c9a";
const GATEWAY_SCOPE: &str = "https://www.wvd.microsoft.com/.default";

#[derive(Default)]
struct Seen {
    entra_forms: Mutex<Vec<HashMap<String, String>>>,
    gateway_heads: Mutex<Vec<String>>,
}

struct Request {
    head: String,
    body: Vec<u8>,
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let length = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Request { head, body })
}

async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Mock Entra token endpoint on plain HTTP. Returns its base URL.
async fn mock_entra(seen: Arc<Seen>) -> String {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let seen = seen.clone();
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut stream).await else { return };
                let form: HashMap<String, String> =
                    url::form_urlencoded::parse(&request.body).into_owned().collect();
                seen.entra_forms.lock().unwrap().push(form);
                let body = serde_json::json!({
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "access_token": FAKE_TOKEN,
                    "refresh_token": "rotated",
                })
                .to_string();
                respond(&mut stream, "200 OK", &body).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Fake AVD gateway: TLS with a self-signed certificate; records the request
/// head and answers 401. Returns its port.
async fn fake_gateway(seen: Arc<Seen>) -> u16 {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::try_from(certified.signing_key.serialize_der()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let (seen, acceptor) = (seen.clone(), acceptor.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else { return };
                let Ok(request) = read_request(&mut tls).await else { return };
                seen.gateway_heads.lock().unwrap().push(request.head);
                respond(&mut tls, "401 Unauthorized", "").await;
            });
        }
    });
    port
}

fn rdp_text(gateway_port: u16) -> String {
    [
        "full address:s:127.0.0.1".to_owned(),
        format!("gatewayhostname:s:127.0.0.1:{gateway_port}"),
        "gatewayusagemethod:i:1".to_owned(),
        "resourceprovider:s:arm".to_owned(),
        "wvd endpoint pool:s:11111111-2222-3333-4444-555555555555".to_owned(),
        "loadbalanceinfo:s:mth://localhost/a/b".to_owned(),
        "remoteapplicationprogram:s:".to_owned(),
        "remoteapplicationmode:i:0".to_owned(),
        "enablerdsaadauth:i:1".to_owned(),
    ]
    .join("\r\n")
}

fn settings() -> ConnectionSettings {
    let mut s = ConnectionSettings::default();
    s.display.mode = DisplayMode::FixedWindow;
    s.display.width = 1024;
    s.display.height = 768;
    s.audio.playback = AudioPlayback::Off;
    s.audio.microphone = false;
    s.devices.clipboard = false;
    s.advanced.certificates = CertPolicy::Ignore;
    s.advanced.hotkeys = HotkeyModifier::Unmanaged;
    s.advanced.auto_reconnect = false;
    s.advanced.grab_keyboard = false;
    s.advanced.log_level = LogLevel::Debug;
    s.advanced.extra_args = "-sdl-connection-dialog".to_owned();
    s
}

struct Outcome {
    code: Option<i32>,
    log: String,
}

async fn run_once(root: &Path, seen: Arc<Seen>, client: &freerdp::Client, child_env: Vec<(String, String)>) -> Outcome {
    let entra = mock_entra(seen.clone()).await;
    let gateway_port = fake_gateway(seen.clone()).await;

    let account = Account {
        tenant_id: "tenant-1".into(),
        object_id: "object-1".into(),
        username: "alex@example.com".into(),
        name: None,
    };
    let http = cloudrdpapp::http_client();
    let auth = AuthManager::new(OAuthClient::with_authority(http.clone(), entra), SecretStore::in_memory());
    auth.set_session(account, "refresh-1").await;

    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let opened_sink = opened.clone();
    let backend = Backend::with_parts(BackendParts {
        auth,
        feed: FeedClient::for_testing(http, "http://127.0.0.1:9/unused"),
        runtime_dir: root.join("run"),
        cache_dir: root.join("cache"),
        log_dir: root.join("logs"),
        sdl_config: root.join("config").join("freerdp").join("sdl-freerdp.json"),
        helper_exe: PathBuf::from(env!("CARGO_BIN_EXE_cloudrdpapp")),
        url_handler_exe: None,
        clients: Clients { sdl: Some(client.clone()), x11: None },
        child_env,
        open_url: Arc::new(move |url: &str| opened_sink.lock().unwrap().push(url.to_owned())),
        status: Arc::new(|text| eprintln!("status: {text}")),
    })
    .await
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::<SessionEvent>::new()));
    let events_sink = events.clone();
    let sink: EventSink = Arc::new(move |event| events_sink.lock().unwrap().push(event));
    let session = backend
        .connect_rdp(&rdp_text(gateway_port), "Integration Desk", &settings(), None, None, sink)
        .await
        .expect("FreeRDP should start");
    assert_eq!(session.client, ClientKind::Sdl);

    let mut code = None;
    let mut exited = false;
    for _ in 0..600 {
        if let Some(SessionEvent::Exited { code: c }) =
            events.lock().unwrap().iter().find(|e| matches!(e, SessionEvent::Exited { .. }))
        {
            code = *c;
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let log = std::fs::read_to_string(session.log_file()).unwrap_or_default();
    assert!(exited, "FreeRDP did not exit within 60 s. Log:\n{log}");
    assert!(opened.lock().unwrap().is_empty(), "no browser may be opened on the silent route");
    drop(session);
    Outcome { code, log }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_gateway_sign_in_with_real_freerdp() {
    cloudrdpapp::install_crypto_provider();
    let Some(client) = freerdp::detect(ClientKind::Sdl, "") else {
        eprintln!("skipped: sdl-freerdp3 is not installed");
        return;
    };
    if !client.supports_helper() {
        eprintln!("skipped: {} is FreeRDP {}, the helper protocol needs 3.32 or later", client.path.display(), client.version_string());
        return;
    }

    let root = std::env::temp_dir().join(format!("cloudrdpapp-it-{}", cloudrdpapp::util::random_token(6)));
    // FreeRDP's own configuration (known hosts, SDL preferences) goes here, not to the user's.
    let isolated = vec![
        ("XDG_CONFIG_HOME".to_owned(), root.join("config").to_string_lossy().into_owned()),
        ("XDG_DATA_HOME".to_owned(), root.join("data").to_string_lossy().into_owned()),
        ("XDG_CACHE_HOME".to_owned(), root.join("xdg-cache").to_string_lossy().into_owned()),
    ];

    let mut seen = Arc::new(Seen::default());
    let mut env = isolated.clone();
    env.push(("SDL_VIDEODRIVER".to_owned(), "dummy".to_owned()));
    let mut outcome = run_once(&root, seen.clone(), &client, env).await;
    if seen.gateway_heads.lock().unwrap().is_empty() && outcome.log.contains("SDL_Init failed") {
        // This SDL build has no dummy driver. The normal driver is safe too:
        // the connection fails at the gateway, before any window exists,
        // and the connection dialog is off.
        eprintln!("SDL dummy video driver unavailable; retrying with the default driver");
        seen = Arc::new(Seen::default());
        outcome = run_once(&root, seen.clone(), &client, isolated).await;
    }
    let Outcome { code, log } = outcome;

    let heads = seen.gateway_heads.lock().unwrap().clone();
    let forms = seen.entra_forms.lock().unwrap().clone();
    let context = format!("exit code {code:?}\ngateway saw {heads:#?}\nEntra saw {forms:#?}\nFreeRDP log:\n{log}");
    if std::env::var_os("CLOUDRDPAPP_IT_VERBOSE").is_some() {
        eprintln!("{context}");
    }

    // The gateway received the minted token on the ARM connection request.
    let head = heads.first().unwrap_or_else(|| panic!("the fake gateway saw no request\n{context}"));
    assert!(head.starts_with("POST /api/arm/v2/connections "), "{context}");
    assert!(
        head.lines().any(|l| l.trim().eq_ignore_ascii_case(&format!("Authorization: Bearer {FAKE_TOKEN}"))),
        "{context}"
    );

    // The token came from the saved refresh token via the mock Entra.
    let grant = forms
        .iter()
        .find(|f| f.get("grant_type").map(String::as_str) == Some("refresh_token"))
        .unwrap_or_else(|| panic!("Entra saw no refresh_token grant\n{context}"));
    assert_eq!(grant.get("refresh_token").map(String::as_str), Some("refresh-1"), "{context}");
    assert!(grant.get("scope").is_some_and(|s| s.split_whitespace().any(|p| p == GATEWAY_SCOPE)), "{context}");
    // No authorization code or other grant ever reaches Entra.
    assert!(forms.iter().all(|f| f.get("grant_type").map(String::as_str) == Some("refresh_token")), "{context}");

    // FreeRDP gave up because of the 401, not earlier.
    assert_ne!(code, Some(0), "{context}");
    let _ = std::fs::remove_dir_all(&root);
}
