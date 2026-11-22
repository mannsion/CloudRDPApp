//! HTTP access to the feed service.

use super::FeedError;
use super::model::{Feed, Resource};
use super::parse::{self, DiscoveredFeed};
use crate::consts;

const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;
const MAX_RDP_BYTES: usize = 1024 * 1024;
const MAX_ICON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FeedClient {
    http: reqwest::Client,
    discovery_url: String,
    user_agent: String,
    /// Skip the Microsoft-host check (tests talk to a local mock).
    trust_all: bool,
}

impl FeedClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            discovery_url: consts::FEED_DISCOVERY_URL.to_owned(),
            user_agent: consts::feed_user_agent(),
            trust_all: false,
        }
    }

    /// Point the client at another discovery URL and disable the host check.
    /// For tests only.
    pub fn for_testing(http: reqwest::Client, discovery_url: impl Into<String>) -> Self {
        Self {
            http,
            discovery_url: discovery_url.into(),
            user_agent: consts::FEED_USER_AGENT.to_owned(),
            trust_all: true,
        }
    }

    /// Download and merge every workspace feed the token is entitled to.
    pub async fn fetch(&self, token: &str) -> Result<Feed, FeedError> {
        let body = self.get(&self.discovery_url, token, consts::ACCEPT_DISCOVERY, MAX_FEED_BYTES).await?;
        let xml = decode_text(&body);
        dump_raw("discovery.xml", &xml);
        let mut discovered = parse::parse_discovery(&xml, &self.discovery_url)?;
        if discovered.is_empty() {
            // Right after sign-in the service sometimes answers with an empty
            // list and fills it in moments later; ask once more before
            // reporting that nothing is assigned.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let body = self.get(&self.discovery_url, token, consts::ACCEPT_DISCOVERY, MAX_FEED_BYTES).await?;
            let xml = decode_text(&body);
            dump_raw("discovery.xml", &xml);
            discovered = parse::parse_discovery(&xml, &self.discovery_url)?;
        }
        if discovered.is_empty() {
            return Err(FeedError::Empty);
        }

        let mut resources = Vec::new();
        let mut warnings = Vec::new();
        let mut unauthorized = false;
        for source in &discovered {
            match self.fetch_one(source, token).await {
                Ok(mut found) => resources.append(&mut found),
                Err(FeedError::Unauthorized) => unauthorized = true,
                Err(err) => {
                    let name = if source.workspace_name.is_empty() { "a workspace" } else { &source.workspace_name };
                    tracing::warn!("feed for {name} failed: {err}");
                    warnings.push(format!("{name}: {err}"));
                }
            }
        }
        if resources.is_empty() {
            if unauthorized {
                return Err(FeedError::Unauthorized);
            }
            if let Some(first) = warnings.first() {
                return Err(FeedError::Malformed(first.clone()));
            }
            return Err(FeedError::Empty);
        }
        Ok(Feed::from_resources(resources, warnings))
    }

    async fn fetch_one(&self, source: &DiscoveredFeed, token: &str) -> Result<Vec<Resource>, FeedError> {
        let body = self.get(&source.feed_url, token, consts::ACCEPT_FEED, MAX_FEED_BYTES).await?;
        let xml = decode_text(&body);
        dump_raw(&format!("feed-{}.xml", crate::util::short_hash(&source.feed_url)), &xml);
        parse::parse_feed(&xml, source)
    }

    /// Download the .rdp connection file of a resource.
    pub async fn download_rdp(&self, token: &str, resource: &Resource) -> Result<String, FeedError> {
        let url = resource.rdp_url.as_deref().ok_or(FeedError::NoRdpFile)?;
        let body = self.get(url, token, "*/*", MAX_RDP_BYTES).await?;
        let text = decode_text(&body);
        if !text.lines().any(|line| line.splitn(3, ':').count() == 3) {
            return Err(FeedError::Malformed("the connection file is empty or not an .rdp file".to_owned()));
        }
        Ok(text)
    }

    /// Download a resource icon.
    pub async fn download_icon(&self, token: &str, url: &str) -> Result<Vec<u8>, FeedError> {
        self.get(url, token, "image/png,image/*;q=0.8", MAX_ICON_BYTES).await
    }

    async fn get(&self, url: &str, token: &str, accept: &str, limit: usize) -> Result<Vec<u8>, FeedError> {
        if !self.trust_all && !consts::is_trusted_url(url) {
            let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned));
            return Err(FeedError::Untrusted(host.unwrap_or_else(|| "an invalid address".to_owned())));
        }
        let mut response = self
            .http
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header("X-MS-User-Agent", &self.user_agent)
            .send()
            .await
            .map_err(|err| FeedError::Network(crate::util::redact(&err.to_string())))?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            if status == 401 {
                return Err(FeedError::Unauthorized);
            }
            let mut err_buf = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| FeedError::Network(crate::util::redact(&err.to_string())))?
            {
                let remaining = (64usize * 1024).saturating_sub(err_buf.len());
                if remaining == 0 {
                    break;
                }
                err_buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            let detail = snippet(&err_buf);
            return Err(match status {
                403 => FeedError::Forbidden(detail),
                _ => FeedError::Http { status, detail },
            });
        }
        if response.content_length().is_some_and(|len| len > limit as u64) {
            return Err(FeedError::Malformed("the response is unexpectedly large".to_owned()));
        }
        let mut body = Vec::with_capacity(
            response.content_length().and_then(|n| usize::try_from(n).ok()).unwrap_or(0).min(limit),
        );
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| FeedError::Network(crate::util::redact(&err.to_string())))?
        {
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(FeedError::Malformed("the response is unexpectedly large".to_owned()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Keep the last raw feed documents for troubleshooting when opted in via
/// `CLOUDRDPAPP_DUMP_FEED=1`. They hold workspace and resource names and URLs,
/// never tokens.
fn dump_raw(name: &str, text: &str) {
    if !std::env::var("CLOUDRDPAPP_DUMP_FEED").is_ok_and(|v| v == "1") {
        return;
    }
    let path = crate::paths::state_dir().join("feed-debug").join(name);
    let _ = crate::paths::write_private_file(&path, text.as_bytes());
}

/// A short, single-line excerpt of an error body.
fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(240).collect();
    if out.is_empty() {
        out.push_str("no details");
    }
    out
}

/// Decode a text body that may be UTF-8 (with or without BOM) or UTF-16LE,
/// the encoding mstsc uses for .rdp files.
pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_encodings() {
        assert_eq!(decode_text(b"full address:s:host"), "full address:s:host");
        assert_eq!(decode_text(b"\xEF\xBB\xBFabc"), "abc");
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("a:i:1".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_text(&utf16), "a:i:1");
    }

    #[test]
    fn snippets_are_single_line() {
        assert_eq!(snippet(b"  line one\r\n  line two "), "line one line two");
        assert_eq!(snippet(b""), "no details");
        assert_eq!(snippet(&vec![b'a'; 1000]).len(), 240);
    }

    #[tokio::test]
    async fn refuses_untrusted_hosts_before_sending() {
        let client = FeedClient::new(crate::http_client());
        let resource = Resource {
            id: "r".into(),
            title: "R".into(),
            kind: super::super::ResourceKind::Desktop,
            workspace: "W".into(),
            tenant_id: String::new(),
            feed_url: "https://rdweb.wvd.microsoft.com/f".into(),
            rdp_url: Some("https://evil.example/steal.rdp".into()),
            icon_url: None,
            folders: vec![],
        };
        let err = client.download_rdp("secret-token", &resource).await.unwrap_err();
        assert!(matches!(err, FeedError::Untrusted(host) if host == "evil.example"));

        let no_file = Resource { rdp_url: None, ..resource };
        assert!(matches!(client.download_rdp("t", &no_file).await, Err(FeedError::NoRdpFile)));
    }

    #[tokio::test]
    async fn enforces_size_limit_from_header_and_chunks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req).await;
                if i == 0 {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 200\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n78\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.write_all(&[b'x'; 120]).await;
                    let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
                }
            }
        });
        let client = FeedClient::for_testing(crate::http_client(), format!("http://{addr}/"));
        let err = client.get(&format!("http://{addr}/a"), "t", "*/*", 100).await.unwrap_err();
        assert!(matches!(err, FeedError::Malformed(ref msg) if msg.contains("unexpectedly large")), "{err}");
        let err = client.get(&format!("http://{addr}/b"), "t", "*/*", 100).await.unwrap_err();
        assert!(matches!(err, FeedError::Malformed(ref msg) if msg.contains("unexpectedly large")), "{err}");
    }
}
