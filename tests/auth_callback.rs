//! Browser sign-in through the broker redirect, end to end with the real
//! executable: `cloudrdpapp --auth-callback <url>` (as the desktop would start
//! it) reaches the waiting sign-in over the helper socket, which redeems the
//! code at a mock Entra token endpoint. Nothing touches the real desktop:
//! the URL handler registration is disabled and `XDG_RUNTIME_DIR` points
//! into a temporary directory.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use cloudrdpapp::auth::store::SecretStore;
use cloudrdpapp::auth::{AuthManager, OAuthClient};
use cloudrdpapp::backend::{Backend, BackendParts, Clients};
use cloudrdpapp::consts;
use cloudrdpapp::feed::FeedClient;
use cloudrdpapp::util;

type Forms = Arc<Mutex<Vec<HashMap<String, String>>>>;

/// Mock token endpoint: records each form, answers with an ID token for
/// alex@example.com.
async fn mock_entra(forms: Forms) -> String {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let forms = forms.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, length) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, length);
                    }
                };
                while buf.len() < head_end + length {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let form: HashMap<String, String> =
                    url::form_urlencoded::parse(&buf[head_end..]).into_owned().collect();
                forms.lock().unwrap().push(form);
                let claims = serde_json::json!({
                    "tid": "tenant-1",
                    "oid": "object-1",
                    "preferred_username": "alex@example.com",
                    "name": "Alex",
                });
                let id_token = format!(
                    "{}.{}.sig",
                    util::b64url_encode(br#"{"alg":"none"}"#),
                    util::b64url_encode(claims.to_string().as_bytes())
                );
                let body = serde_json::json!({
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "id_token": id_token,
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

async fn run_callback(xdg_runtime: &Path, url: &str) -> Output {
    let (dir, url) = (xdg_runtime.to_path_buf(), url.to_owned());
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_cloudrdpapp"))
            .arg("--auth-callback")
            .arg(&url)
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_callback_completes_a_pending_sign_in() {
    cloudrdpapp::install_crypto_provider();
    let root = std::env::temp_dir().join(format!("cloudrdpapp-cb-{}", util::random_token(6)));
    let xdg_runtime = root.join("xdg-runtime");
    let forms: Forms = Arc::default();
    let entra = mock_entra(forms.clone()).await;

    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let opened_sink = opened.clone();
    let http = cloudrdpapp::http_client();
    let backend = Backend::with_parts(BackendParts {
        auth: AuthManager::new(OAuthClient::with_authority(http.clone(), entra), SecretStore::in_memory()),
        feed: FeedClient::for_testing(http, "http://127.0.0.1:9/unused"),
        // What paths::runtime_dir() resolves to for the callback process.
        runtime_dir: xdg_runtime.join(consts::APP_DIR),
        cache_dir: root.join("cache"),
        log_dir: root.join("logs"),
        sdl_config: root.join("sdl-freerdp.json"),
        helper_exe: PathBuf::from(env!("CARGO_BIN_EXE_cloudrdpapp")),
        url_handler_exe: None,
        clients: Clients::default(),
        child_env: vec![],
        open_url: Arc::new(move |url: &str| opened_sink.lock().unwrap().push(url.to_owned())),
        status: Arc::new(|text| eprintln!("status: {text}")),
    })
    .await
    .unwrap();

    // Only the owner may use the socket.
    let socket = std::fs::read_dir(xdg_runtime.join(consts::APP_DIR))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("helper-"))
        .expect("helper socket");
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let signing_in = tokio::spawn({
        let backend = backend.clone();
        async move { backend.sign_in().await }
    });
    // Wait for the "browser" to be opened.
    let authorize = loop {
        if let Some(url) = opened.lock().unwrap().first().cloned() {
            break url;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let query: HashMap<String, String> = url::Url::parse(&authorize).unwrap().query_pairs().into_owned().collect();
    let broker = format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{}", consts::CLIENT_ID);
    assert_eq!(query["redirect_uri"], broker);
    let state = query["state"].clone();

    // Junk and foreign responses are refused by the callback process itself
    // or by the app, and do not end the sign-in.
    let foreign = run_callback(&xdg_runtime, "https://evil.example/?code=x&state=y").await;
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("refusing"), "{foreign:?}");
    let wrong_state = run_callback(&xdg_runtime, &format!("{broker}?code=stolen&state=not-{state}")).await;
    assert!(!wrong_state.status.success());
    assert!(String::from_utf8_lossy(&wrong_state.stderr).contains("not accepted"), "{wrong_state:?}");
    assert!(!signing_in.is_finished());

    // The browser hands the broker redirect to the desktop, which runs us.
    // (Browsers may change the case of scheme and host.)
    let callback = format!("ms-appx-web://microsoft.aad.brokerplugin/{}?code=the-code&state={state}", consts::CLIENT_ID);
    let delivered = run_callback(&xdg_runtime, &callback).await;
    assert!(delivered.status.success(), "{delivered:?}");

    let account = tokio::time::timeout(Duration::from_secs(10), signing_in).await.unwrap().unwrap().unwrap();
    assert_eq!(account.username, "alex@example.com");
    assert_eq!(backend.account().await, Some(account));

    let forms = forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1, "{forms:?}");
    assert_eq!(forms[0]["grant_type"], "authorization_code");
    assert_eq!(forms[0]["code"], "the-code");
    assert_eq!(forms[0]["redirect_uri"], broker);
    assert!(!forms[0]["code_verifier"].is_empty());

    // The same response cannot be used twice.
    let replay = run_callback(&xdg_runtime, &callback).await;
    assert!(!replay.status.success());

    drop(backend);
    let _ = std::fs::remove_dir_all(&root);
}
