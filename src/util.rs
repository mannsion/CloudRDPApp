//! Small shared helpers: randomness, encoding, redaction.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).expect("the operating system random source is unavailable");
    buf
}

/// Random token made of URL-safe base64 characters.
pub fn random_token(bytes: usize) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(bytes))
}

pub fn b64url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Decode base64url, tolerating padding and the standard alphabet.
pub fn b64url_decode(data: &str) -> Option<Vec<u8>> {
    let trimmed = data.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()
        .or_else(|| STANDARD.decode(data.trim()).ok())
}

pub fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Short stable hex digest, used for file names and settings keys.
pub fn short_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    hex(&digest[..8])
}

/// Percent-encode everything except unreserved characters (RFC 3986).
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turn a title into something safe to use as a file name.
pub fn file_safe(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let mut out: String = trimmed.chars().take(60).collect();
    if out.is_empty() {
        out.push_str("resource");
    }
    out
}

/// Remove bearer tokens, authorization codes and similar secrets from text
/// before it is logged or shown in the UI.
pub fn redact(input: &str) -> String {
    const KEYS: &[&str] = &[
        "code=",
        "code_verifier=",
        "refresh_token=",
        "access_token=",
        "id_token=",
        "client_info=",
        "session_state=",
        "bearer:",
        "/p:",
        "password=",
        "client_secret=",
    ];
    let mut out = input.to_owned();
    for key in KEYS {
        let mut search_from = 0;
        while let Some(rel) = out[search_from..].find(key) {
            let start = search_from + rel + key.len();
            let end = out[start..]
                .find(|c: char| c == '&' || c == ',' || c.is_whitespace() || c == '"' || c == '\'')
                .map(|i| start + i)
                .unwrap_or(out.len());
            if end > start {
                out.replace_range(start..end, "<redacted>");
                search_from = start + "<redacted>".len();
            } else {
                search_from = start;
            }
            if search_from >= out.len() {
                break;
            }
        }
    }
    out = redact_bearer_headers(&out);
    out = redact_json_secrets(&out);
    redact_jwts(&out)
}

fn redact_bearer_headers(input: &str) -> String {
    const NEEDLE: &[u8] = b"bearer";
    let mut out = input.to_owned();
    let mut search_from = 0;
    while search_from + NEEDLE.len() < out.len() {
        let bytes = out.as_bytes();
        let Some(rel) = bytes[search_from..]
            .windows(NEEDLE.len())
            .position(|w| w.eq_ignore_ascii_case(NEEDLE))
        else {
            break;
        };
        let pos = search_from + rel;
        let boundary_ok = pos == 0 || !(bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_');
        let after_keyword = pos + NEEDLE.len();
        if boundary_ok && let Some(rest) = out.get(after_keyword..) {
            let ws_len = rest.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count();
            if ws_len > 0 {
                let start = after_keyword + ws_len;
                let end = out[start..]
                    .find(|c: char| c == '&' || c == ',' || c == ';' || c.is_whitespace() || c == '"' || c == '\'')
                    .map(|i| start + i)
                    .unwrap_or(out.len());
                if end > start {
                    out.replace_range(start..end, "<redacted>");
                    search_from = start + "<redacted>".len();
                    continue;
                }
            }
        }
        search_from = after_keyword;
    }
    out
}

fn redact_json_secrets(input: &str) -> String {
    const JSON_KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "id_token",
        "client_info",
        "code",
        "code_verifier",
        "password",
        "client_secret",
    ];
    let mut out = input.to_owned();
    for key in JSON_KEYS {
        let quoted = format!("\"{key}\"");
        let mut search_from = 0;
        while let Some(rel) = out[search_from..].find(&quoted) {
            let after_key = search_from + rel + quoted.len();
            let mut cursor = after_key;
            cursor += out[cursor..].chars().take_while(|c| c.is_whitespace()).map(char::len_utf8).sum::<usize>();
            if !out[cursor..].starts_with(':') {
                search_from = after_key;
                continue;
            }
            cursor += 1;
            cursor += out[cursor..].chars().take_while(|c| c.is_whitespace()).map(char::len_utf8).sum::<usize>();
            if !out[cursor..].starts_with('"') {
                search_from = after_key;
                continue;
            }
            let start = cursor + 1;
            let mut end = None;
            let mut escaped = false;
            for (idx, ch) in out[start..].char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    end = Some(start + idx);
                    break;
                }
            }
            if let Some(end) = end
                && end > start
            {
                out.replace_range(start..end, "<redacted>");
                search_from = start + "<redacted>".len();
            } else {
                search_from = after_key;
            }
        }
    }
    out
}

/// Replace anything that looks like a JWT (`eyJ….….…`) with a placeholder.
fn redact_jwts(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("eyJ") {
        out.push_str(&rest[..pos]);
        let candidate = &rest[pos..];
        let end = candidate
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
            .unwrap_or(candidate.len());
        let token = &candidate[..end];
        if token.len() > 40 && token.matches('.').count() >= 2 {
            out.push_str("<jwt>");
        } else {
            out.push_str(token);
        }
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

/// Split a string the way a shell would (quotes and backslash escapes), for
/// the "extra FreeRDP arguments" setting.
pub fn shell_split(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => match chars.peek() {
                Some('"') | Some('\\') => current.push(chars.next().unwrap()),
                _ => current.push(c),
            },
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_token = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        in_token = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                c => {
                    current.push(c);
                    in_token = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".to_owned());
    }
    if in_token {
        args.push(current);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_tokens_differ() {
        let a = random_token(32);
        let b = random_token(32);
        assert_ne!(a, b);
        assert!(a.len() >= 42);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn b64url_roundtrip() {
        let data = b"\xff\xfe hello";
        assert_eq!(b64url_decode(&b64url_encode(data)).unwrap(), data);
        // Padded input is accepted too.
        assert_eq!(b64url_decode("aGk=").unwrap(), b"hi");
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("http://localhost:80/"), "http%3A%2F%2Flocalhost%3A80%2F");
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    #[test]
    fn redacts_secrets() {
        let line = "GET /?code=0.AAAA-secret&state=abc HTTP/1.1";
        let out = redact(line);
        assert!(!out.contains("secret"));
        assert!(out.contains("state=abc"));

        let args = "/gateway:type:arm,bearer:eyJ0eXAi.payload.sig /u:me";
        assert!(!redact(args).contains("payload"));

        let jwt = format!("token {}.{}.{}", "eyJ".to_owned() + &"a".repeat(30), "b".repeat(30), "c".repeat(30));
        assert_eq!(redact(&jwt), "token <jwt>");

        let header = "Authorization: Bearer opaque-secret-token\r\nX-Other: ok";
        let out = redact(header);
        assert_eq!(out, "Authorization: Bearer <redacted>\r\nX-Other: ok");
        assert_eq!(redact("authorization: bearer short-tok, next"), "authorization: bearer <redacted>, next");

        let json = r#"{"token_type":"Bearer","access_token":"AT-opaque","refresh_token": "RT-secret","id_token":"ID-tok","client_info":"ci","code":"c1","code_verifier":"cv","password":"pw\"1","client_secret" : "cs","expires_in":3599}"#;
        let out = redact(json);
        for leaked in ["AT-opaque", "RT-secret", "ID-tok", "ci", "c1", "cv", "pw\\\"1", "cs"] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
        assert!(out.contains(r#""token_type":"Bearer""#), "{out}");
        assert!(out.contains(r#""expires_in":3599"#), "{out}");
    }

    #[test]
    fn redact_leaves_plain_text_alone() {
        assert_eq!(redact("nothing to see here"), "nothing to see here");
    }

    #[test]
    fn shell_splitting() {
        assert_eq!(shell_split("/gfx /network:auto").unwrap(), vec!["/gfx", "/network:auto"]);
        assert_eq!(
            shell_split(r#"/drive:home,"/my folder" -wallpaper"#).unwrap(),
            vec!["/drive:home,/my folder", "-wallpaper"]
        );
        assert_eq!(shell_split("  ").unwrap(), Vec::<String>::new());
        assert_eq!(shell_split("''").unwrap(), vec![""]);
        assert!(shell_split("'open").is_err());
    }

    #[test]
    fn file_safe_names() {
        assert_eq!(file_safe("My Cloud PC (West)"), "My_Cloud_PC__West");
        assert_eq!(file_safe("///"), "resource");
    }
}
