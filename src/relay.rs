//! Local token relay: lets FreeRDP obtain its connection-time tokens from
//! the saved sign-in instead of opening a browser for every connection.
//!
//! FreeRDP signs in to the gateway and to the session host with an OAuth
//! authorization-code flow against a configurable Entra authority
//! (`/azure:ad:<host>`). It reads the authority's OpenID configuration,
//! "navigates" to the authorize endpoint through its sign-in helper, then
//! posts the code to the token endpoint, adding `req_cnf` so the session-host
//! token is bound to a key only FreeRDP holds.
//!
//! This module is that authority, on `127.0.0.1`:
//!
//! * the helper hands FreeRDP a one-time code issued by [`TokenRelay::issue_code`];
//! * FreeRDP redeems it here; the relay performs the equivalent
//!   `refresh_token` grant against the real Entra ID (forwarding `req_cnf`)
//!   and returns Entra's answer;
//! * every other token request (FreeRDP's `srv_challenge` nonce request) is
//!   forwarded unchanged.
//!
//! FreeRDP's HTTPS client only accepts `https://` URLs and verifies the
//! certificate chain against OpenSSL's default trust store, so the relay
//! serves TLS with an ephemeral self-signed certificate. The FreeRDP child is
//! started with `SSL_CERT_FILE` pointing at a bundle of the system roots plus
//! that certificate. The private key never leaves this process.
//!
//! The TLS server is OpenSSL, not rustls: FreeRDP sends the whole
//! `host:port` of the authority as its SNI value (`libfreerdp/utils/http.c`
//! passes the URL's authority to `SSL_set_tlsext_host_name`), and rustls
//! rejects such a ClientHello with an `illegal_parameter` alert.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openssl::pkey::PKey;
use openssl::ssl::{Ssl, SslAcceptor, SslMethod};
use openssl::x509::X509;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::auth::AuthManager;
use crate::auth::manager::cache_key;
use crate::paths;
use crate::util;

/// One-time codes must be redeemed quickly; FreeRDP does so immediately.
const CODE_LIFETIME: Duration = Duration::from_secs(120);
const MAX_REQUEST: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct TokenRelay {
    inner: Arc<Inner>,
}

struct Inner {
    addr: SocketAddr,
    ca_bundle: PathBuf,
    auth: AuthManager,
    codes: Mutex<HashMap<String, IssuedCode>>,
}

struct IssuedCode {
    scope_key: String,
    issued: Instant,
}

impl TokenRelay {
    /// Start listening on a free loopback port. Must be called inside a
    /// Tokio runtime; the accept loop runs until the runtime shuts down.
    pub async fn start(auth: AuthManager, runtime_dir: &Path) -> Result<Self, String> {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .map_err(|err| format!("could not create a certificate for the token relay: {err}"))?;
        let cert_pem = certified.cert.pem();
        let acceptor = tls_acceptor(certified.cert.der(), &certified.signing_key.serialize_der())
            .map_err(|err| format!("token relay TLS: {err}"))?;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|err| format!("could not open a local port for the token relay: {err}"))?;
        let addr = listener.local_addr().map_err(|err| err.to_string())?;

        paths::create_private_dir(runtime_dir).map_err(|err| format!("{}: {err}", runtime_dir.display()))?;
        let ca_bundle = runtime_dir.join(format!("ca-bundle-{}.pem", std::process::id()));
        paths::write_private_file(&ca_bundle, build_ca_bundle(&cert_pem).as_bytes())
            .map_err(|err| format!("{}: {err}", ca_bundle.display()))?;

        let inner = Arc::new(Inner { addr, ca_bundle, auth, codes: Mutex::new(HashMap::new()) });
        let relay = Self { inner };

        let accept_relay = relay.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let relay = accept_relay.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let served = tokio::time::timeout(IO_TIMEOUT * 3, async {
                        let ssl = Ssl::new(acceptor.context()).map_err(|err| err.to_string())?;
                        let mut tls = tokio_openssl::SslStream::new(ssl, stream).map_err(|err| err.to_string())?;
                        std::pin::Pin::new(&mut tls).accept().await.map_err(|err| err.to_string())?;
                        relay.serve(&mut tls).await
                    })
                    .await;
                    match served {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::debug!("token relay connection ended: {err}"),
                        Err(_) => tracing::debug!("token relay connection timed out"),
                    }
                });
            }
        });
        Ok(relay)
    }

    /// `host:port` to pass to FreeRDP as `/azure:ad:`.
    pub fn authority(&self) -> String {
        format!("127.0.0.1:{}", self.inner.addr.port())
    }

    /// Certificate bundle for the FreeRDP child's `SSL_CERT_FILE`.
    pub fn ca_bundle(&self) -> &Path {
        &self.inner.ca_bundle
    }

    /// Issue a one-time authorization code for `scope`. Whoever presents it
    /// to the token endpoint receives a token for exactly that scope.
    pub fn issue_code(&self, scope: &str) -> String {
        let code = util::random_token(32);
        let mut codes = self.inner.codes.lock().unwrap();
        codes.retain(|_, issued| issued.issued.elapsed() < CODE_LIFETIME);
        codes.insert(code.clone(), IssuedCode { scope_key: cache_key(scope), issued: Instant::now() });
        code
    }

    fn redeem_code(&self, code: &str, scope: &str) -> bool {
        let mut codes = self.inner.codes.lock().unwrap();
        match codes.remove(code) {
            Some(issued) => issued.issued.elapsed() < CODE_LIFETIME && issued.scope_key == cache_key(scope),
            None => false,
        }
    }

    async fn serve<S>(&self, stream: &mut S) -> Result<(), String>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let request = read_request(stream).await?;
        let (status, body) = self.handle(&request).await;
        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            reason(status),
            body.len()
        );
        stream.write_all(response.as_bytes()).await.map_err(|err| err.to_string())?;
        stream.write_all(body.as_bytes()).await.map_err(|err| err.to_string())?;
        stream.flush().await.map_err(|err| err.to_string())?;
        let _ = stream.shutdown().await;
        Ok(())
    }

    async fn handle(&self, request: &Request) -> (u16, String) {
        let path = request.target.split('?').next().unwrap_or("");
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", [tenant, "v2.0", ".well-known", "openid-configuration"]) => {
                if !valid_tenant(tenant) {
                    return (400, error_json("invalid_request", "invalid tenant"));
                }
                let base = format!("https://{}/{tenant}", self.authority());
                let doc = serde_json::json!({
                    "issuer": format!("{base}/v2.0"),
                    "authorization_endpoint": format!("{base}/oauth2/v2.0/authorize"),
                    "token_endpoint": format!("{base}/oauth2/v2.0/token"),
                    "response_types_supported": ["code"],
                });
                (200, doc.to_string())
            }
            ("POST", [tenant, "oauth2", "v2.0", "token"]) => {
                if !valid_tenant(tenant) {
                    return (400, error_json("invalid_request", "invalid tenant"));
                }
                self.token(tenant, &request.body).await
            }
            _ => (404, error_json("not_found", "unknown endpoint")),
        }
    }

    async fn token(&self, tenant: &str, body: &[u8]) -> (u16, String) {
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body).into_owned().collect();
        let field = |name: &str| form.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());

        match field("grant_type") {
            Some("authorization_code") => {
                let (Some(code), Some(scope)) = (field("code"), field("scope")) else {
                    return (400, error_json("invalid_request", "code and scope are required"));
                };
                if !self.redeem_code(code, scope) {
                    return (400, error_json("invalid_grant", "the authorization code is unknown, expired or was issued for another scope"));
                }
                match self.inner.auth.token_response(scope, field("req_cnf")).await {
                    Ok((status, body)) => {
                        if status != 200 {
                            tracing::warn!("Entra ID refused a token for {}: {}", scope, util::redact(&body));
                        }
                        (status, body)
                    }
                    Err(err) => {
                        tracing::warn!("could not mint a token for {scope}: {err}");
                        (400, error_json("interaction_required", &err.to_string()))
                    }
                }
            }
            // FreeRDP asks Entra for a server nonce before it proves
            // possession of its key. Nothing secret is involved.
            Some("srv_challenge") => {
                let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                match self.inner.auth.oauth().post_form(tenant, &pairs).await {
                    Ok((status, body)) => (status, body),
                    Err(err) => (502, error_json("temporarily_unavailable", &err.to_string())),
                }
            }
            _ => (400, error_json("unsupported_grant_type", "only authorization_code and srv_challenge are relayed")),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.ca_bundle);
    }
}

struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

async fn read_request<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<Request, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_REQUEST {
            return Err("request head too large".into());
        }
        let n = tokio::time::timeout(IO_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| "timed out reading the request".to_owned())?
            .map_err(|err| err.to_string())?;
        if n == 0 {
            return Err("connection closed before the request was complete".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_owned();
    let target = request_line.next().unwrap_or("").to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST {
        return Err("request body too large".into());
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let n = tokio::time::timeout(IO_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| "timed out reading the request body".to_owned())?
            .map_err(|err| err.to_string())?;
        if n == 0 {
            return Err("connection closed inside the request body".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Request { method, target, body })
}

fn tls_acceptor(cert_der: &[u8], key_der: &[u8]) -> Result<SslAcceptor, openssl::error::ErrorStack> {
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
    let cert = X509::from_der(cert_der)?;
    let key = PKey::private_key_from_der(key_der)?;
    builder.set_certificate(&cert)?;
    builder.set_private_key(&key)?;
    builder.check_private_key()?;
    Ok(builder.build())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn valid_tenant(tenant: &str) -> bool {
    !tenant.is_empty()
        && tenant.len() <= 128
        && tenant != "."
        && tenant != ".."
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn error_json(error: &str, description: &str) -> String {
    serde_json::json!({ "error": error, "error_description": description }).to_string()
}

/// System trust roots followed by the relay certificate.
fn build_ca_bundle(relay_cert_pem: &str) -> String {
    let system = std::env::var_os("SSL_CERT_FILE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| openssl_probe::probe().cert_file)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    if system.is_empty() {
        tracing::warn!("no system certificate bundle found; FreeRDP may fail to verify the gateway");
    }
    let mut bundle = system;
    if !bundle.is_empty() && !bundle.ends_with('\n') {
        bundle.push('\n');
    }
    bundle.push_str(relay_cert_pem);
    if !bundle.ends_with('\n') {
        bundle.push('\n');
    }
    bundle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OAuthClient;
    use crate::auth::store::SecretStore;

    fn manager() -> AuthManager {
        AuthManager::new(OAuthClient::new(crate::http_client()), SecretStore::in_memory())
    }

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cloudrdpapp-relay-{}", util::random_token(6)))
    }

    #[tokio::test]
    async fn codes_are_single_use_and_scope_bound() {
        let dir = test_dir();
        let relay = TokenRelay::start(manager(), &dir).await.unwrap();
        let scope = "https://www.wvd.microsoft.com/.default openid profile offline_access";
        let code = relay.issue_code(scope);
        assert!(!relay.redeem_code(&code, "ms-device-service://termsrv.wvd.microsoft.com/name/h/user_impersonation"));
        // The failed attempt consumed it.
        assert!(!relay.redeem_code(&code, scope));

        let code = relay.issue_code(scope);
        assert!(relay.redeem_code(&code, "https://www.wvd.microsoft.com/.default"));
        assert!(!relay.redeem_code(&code, scope));
        assert!(!relay.redeem_code("made-up", scope));
        drop(relay);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bundle_contains_the_relay_certificate() {
        let dir = test_dir();
        let relay = TokenRelay::start(manager(), &dir).await.unwrap();
        let bundle = std::fs::read_to_string(relay.ca_bundle()).unwrap();
        assert!(bundle.trim_end().ends_with("-----END CERTIFICATE-----"));
        assert!(bundle.matches("BEGIN CERTIFICATE").count() >= 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(relay.ca_bundle()).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(relay.authority().starts_with("127.0.0.1:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn routes_requests() {
        let dir = test_dir();
        let relay = TokenRelay::start(manager(), &dir).await.unwrap();

        let (status, body) = relay
            .handle(&Request { method: "GET".into(), target: "/common/v2.0/.well-known/openid-configuration".into(), body: vec![] })
            .await;
        assert_eq!(status, 200);
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        let base = format!("https://{}/common", relay.authority());
        assert_eq!(doc["token_endpoint"], format!("{base}/oauth2/v2.0/token"));
        assert_eq!(doc["authorization_endpoint"], format!("{base}/oauth2/v2.0/authorize"));

        let (status, _) = relay.handle(&Request { method: "GET".into(), target: "/".into(), body: vec![] }).await;
        assert_eq!(status, 404);

        let post = |body: &str| Request { method: "POST".into(), target: "/common/oauth2/v2.0/token".into(), body: body.as_bytes().to_vec() };
        // Unknown code.
        let (status, body) = relay.handle(&post("grant_type=authorization_code&code=nope&scope=x")).await;
        assert_eq!(status, 400);
        assert!(body.contains("invalid_grant"));
        // Password or refresh grants are never relayed.
        let (status, body) = relay.handle(&post("grant_type=refresh_token&refresh_token=x")).await;
        assert_eq!(status, 400);
        assert!(body.contains("unsupported_grant_type"));
        // A valid code without a signed-in account reports that interaction is needed.
        let code = relay.issue_code("scope-a");
        let (status, body) = relay.handle(&post(&format!("grant_type=authorization_code&code={code}&scope=scope-a"))).await;
        assert_eq!(status, 400);
        assert!(body.contains("interaction_required"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepts_freerdp_style_sni_and_is_trusted_through_the_bundle() {
        let dir = test_dir();
        let relay = TokenRelay::start(manager(), &dir).await.unwrap();
        let authority = relay.authority();
        let bundle = relay.ca_bundle().to_owned();
        let answer = tokio::task::spawn_blocking(move || {
            use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
            use std::io::{Read, Write};
            let mut builder = SslConnector::builder(SslMethod::tls_client()).unwrap();
            builder.set_ca_file(&bundle).unwrap();
            builder.set_verify(SslVerifyMode::PEER);
            let tcp = std::net::TcpStream::connect(&authority).unwrap();
            // FreeRDP sends the URL's whole authority, port included, as SNI
            // and does not check the host name.
            let mut tls = builder
                .build()
                .configure()
                .unwrap()
                .verify_hostname(false)
                .connect(&authority, tcp)
                .unwrap();
            let request = format!("GET /common/v2.0/.well-known/openid-configuration HTTP/1.1\r\nHost: {authority}\r\n\r\n");
            tls.write_all(request.as_bytes()).unwrap();
            let mut answer = String::new();
            let _ = tls.read_to_string(&mut answer);
            answer
        })
        .await
        .unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("Content-Length: ") && answer.contains("token_endpoint"), "{answer}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parses_http_requests() {
        let raw = b"POST /common/oauth2/v2.0/token HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 11\r\n\r\ngrant_type=";
        let mut cursor = std::io::Cursor::new(raw.to_vec());
        let request = read_request(&mut cursor).await.unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/common/oauth2/v2.0/token");
        assert_eq!(request.body, b"grant_type=");

        let mut truncated = std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: x".to_vec());
        assert!(read_request(&mut truncated).await.is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_tenant_segments() {
        let dir = test_dir();
        let relay = TokenRelay::start(manager(), &dir).await.unwrap();
        let long_tenant = "a".repeat(129);
        for bad in [".", "..", "a%2Fb", "bad$tenant", &long_tenant] {
            let (status, body) = relay
                .handle(&Request {
                    method: "GET".into(),
                    target: format!("/{bad}/v2.0/.well-known/openid-configuration"),
                    body: vec![],
                })
                .await;
            assert_eq!(status, 400, "accepted {bad}");
            assert!(body.contains("invalid_request"), "{body}");

            let (status, body) = relay
                .handle(&Request {
                    method: "POST".into(),
                    target: format!("/{bad}/oauth2/v2.0/token"),
                    body: b"grant_type=srv_challenge".to_vec(),
                })
                .await;
            assert_eq!(status, 400, "accepted {bad} on POST");
            assert!(body.contains("invalid_request"), "{body}");
        }
        assert!(valid_tenant("contoso.onmicrosoft.com"));
        assert!(valid_tenant("11111111-2222-3333-4444-555555555555"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
