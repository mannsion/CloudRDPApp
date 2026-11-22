//! OAuth 2.0 requests against the Entra ID v2 endpoints.

use serde::Deserialize;

use super::AuthError;
use crate::consts;
use crate::util;

/// Successful response of the token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default, deserialize_with = "number_or_string")]
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Entra returns `expires_in` as a number on v2 and as a string on v1.
fn number_or_string<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Text(String),
    }
    Ok(match Either::deserialize(de)? {
        Either::Num(n) => n,
        Either::Text(s) => s.trim().parse().unwrap_or(0),
    })
}

/// Error response of the token or authorize endpoint.
#[derive(Debug, Clone, Default, Deserialize, thiserror::Error)]
#[error("{}", self.summary())]
pub struct OAuthError {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_description: String,
    #[serde(default)]
    pub error_codes: Vec<u32>,
    #[serde(default)]
    pub suberror: Option<String>,
    /// Claims challenge issued by Conditional Access.
    #[serde(default)]
    pub claims: Option<String>,
}

impl OAuthError {
    pub fn new(error: impl Into<String>, description: impl Into<String>) -> Self {
        Self { error: error.into(), error_description: description.into(), ..Default::default() }
    }

    /// The first line of the description, which is the human readable part
    /// (the rest is trace and correlation ids).
    pub fn summary(&self) -> String {
        let first = self.error_description.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            if self.error.is_empty() { "sign-in failed".to_owned() } else { self.error.clone() }
        } else {
            first.to_owned()
        }
    }

    /// The `AADSTS…` code from the description, if any.
    pub fn aadsts(&self) -> Option<String> {
        let start = self.error_description.find("AADSTS")?;
        let digits: String = self.error_description[start + 6..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        (!digits.is_empty()).then(|| format!("AADSTS{digits}"))
    }

    /// True when a fresh interactive sign-in can resolve the error: the
    /// refresh token expired or was revoked, MFA is required, or Conditional
    /// Access asks for a recent sign-in.
    pub fn needs_interaction(&self) -> bool {
        // Blocked outright by policy: signing in again does not help.
        const HARD_BLOCKS: &[u32] = &[53003, 53000, 53001, 53002, 50105, 50126];
        if self.error_codes.iter().any(|code| HARD_BLOCKS.contains(code)) {
            return false;
        }
        matches!(self.error.as_str(), "interaction_required" | "invalid_grant" | "consent_required" | "login_required")
    }
}

/// Parameters of an authorization request.
#[derive(Debug, Clone, Default)]
pub struct AuthorizeParams<'a> {
    pub redirect_uri: &'a str,
    pub scope: &'a str,
    pub state: &'a str,
    pub code_challenge: &'a str,
    pub login_hint: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub claims: Option<&'a str>,
}

/// PKCE verifier and S256 challenge (RFC 7636).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        use sha2::{Digest, Sha256};
        let verifier = util::random_token(64);
        let challenge = util::b64url_encode(&Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// Thin client for the Entra ID authorize and token endpoints.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    /// Base URL of the authority, e.g. `https://login.microsoftonline.com`.
    authority: String,
    client_id: String,
}

impl OAuthClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_authority(http, format!("https://{}", consts::AUTHORITY_HOST))
    }

    /// Use a different authority base URL (tests point this at a mock).
    pub fn with_authority(http: reqwest::Client, authority: impl Into<String>) -> Self {
        Self {
            http,
            authority: authority.into().trim_end_matches('/').to_owned(),
            client_id: consts::CLIENT_ID.to_owned(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn authorize_endpoint(&self, tenant: &str) -> String {
        format!("{}/{}/oauth2/v2.0/authorize", self.authority, tenant)
    }

    pub fn token_endpoint(&self, tenant: &str) -> String {
        format!("{}/{}/oauth2/v2.0/token", self.authority, tenant)
    }

    /// Build the URL the browser is sent to.
    pub fn authorize_url(&self, tenant: &str, params: &AuthorizeParams<'_>) -> String {
        let mut url = url::Url::parse(&self.authorize_endpoint(tenant)).expect("valid authority");
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("client_id", &self.client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", params.redirect_uri)
                .append_pair("response_mode", "query")
                .append_pair("scope", params.scope)
                .append_pair("state", params.state)
                .append_pair("code_challenge", params.code_challenge)
                .append_pair("code_challenge_method", "S256");
            if let Some(hint) = params.login_hint.filter(|h| !h.is_empty()) {
                query.append_pair("login_hint", hint);
            }
            if let Some(prompt) = params.prompt.filter(|p| !p.is_empty()) {
                query.append_pair("prompt", prompt);
            }
            if let Some(claims) = params.claims.filter(|c| !c.is_empty()) {
                query.append_pair("claims", claims);
            }
        }
        url.into()
    }

    /// Redeem an authorization code.
    pub async fn exchange_code(
        &self,
        tenant: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        scope: &str,
    ) -> Result<TokenResponse, AuthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", scope),
        ];
        self.token_request(tenant, &form).await
    }

    /// Mint a new access token from a refresh token. `extra` carries
    /// additional form fields such as `req_cnf` for proof-of-possession
    /// tokens.
    pub async fn refresh(
        &self,
        tenant: &str,
        refresh_token: &str,
        scope: &str,
        extra: &[(&str, &str)],
    ) -> Result<TokenResponse, AuthError> {
        let (status, body) = self.refresh_raw(tenant, refresh_token, scope, extra).await?;
        parse_token_body(status, &body)
    }

    /// Like [`refresh`](Self::refresh) but returns the raw status and body,
    /// so the response can be relayed unchanged.
    pub async fn refresh_raw(
        &self,
        tenant: &str,
        refresh_token: &str,
        scope: &str,
        extra: &[(&str, &str)],
    ) -> Result<(u16, String), AuthError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", refresh_token),
            ("scope", scope),
        ];
        form.extend_from_slice(extra);
        self.post_form(tenant, &form).await
    }

    /// Forward an arbitrary form to the token endpoint (used for FreeRDP's
    /// `srv_challenge` nonce request).
    pub async fn post_form(&self, tenant: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
        let response = self.http.post(self.token_endpoint(tenant)).form(form).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        Ok((status, body))
    }

    async fn token_request(&self, tenant: &str, form: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        let (status, body) = self.post_form(tenant, form).await?;
        parse_token_body(status, &body)
    }
}

/// Interpret a token endpoint response.
pub fn parse_token_body(status: u16, body: &str) -> Result<TokenResponse, AuthError> {
    if (200..300).contains(&status) {
        serde_json::from_str::<TokenResponse>(body)
            .map_err(|err| AuthError::Other(format!("unexpected token response: {err}")))
    } else {
        match serde_json::from_str::<OAuthError>(body) {
            Ok(err) if !err.error.is_empty() => Err(AuthError::OAuth(err)),
            _ => Err(AuthError::Other(format!("token endpoint returned HTTP {status}"))),
        }
    }
}

/// What the browser was redirected back with.
#[derive(Debug, Clone, PartialEq)]
pub struct RedirectResult {
    pub code: String,
    pub state: Option<String>,
}

/// Parse a redirect back from the authorize endpoint. Accepts a full URL, a
/// bare query string, or (for the paste fallback) a bare authorization code.
pub fn parse_redirect(input: &str) -> Result<RedirectResult, AuthError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(AuthError::Other("nothing was pasted".to_owned()));
    }
    let query: String = if let Ok(url) = url::Url::parse(input) {
        match (url.query(), url.fragment()) {
            (Some(q), _) if !q.is_empty() => q.to_owned(),
            (_, Some(f)) if !f.is_empty() => f.to_owned(),
            _ => String::new(),
        }
    } else {
        input.trim_start_matches(['?', '#']).to_owned()
    };

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = String::new();
    let mut suberror = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = value.into_owned(),
            "error_subcode" => suberror = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        if error == "access_denied" && suberror.as_deref() == Some("cancel") {
            return Err(AuthError::Cancelled);
        }
        return Err(AuthError::OAuth(OAuthError { error, error_description: description, suberror, ..Default::default() }));
    }
    match code {
        Some(code) if !code.is_empty() => Ok(RedirectResult { code, state }),
        // A bare code pasted on its own: no '=' anywhere.
        _ if !query.contains('=') && !query.is_empty() && url::Url::parse(input).is_err() => {
            Ok(RedirectResult { code: query, state: None })
        }
        _ => Err(AuthError::Other("the address does not contain an authorization code".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OAuthClient {
        OAuthClient::new(crate::http_client())
    }

    #[test]
    fn authorize_url_has_expected_parameters() {
        let pkce = Pkce::new();
        let url = client().authorize_url(
            "organizations",
            &AuthorizeParams {
                redirect_uri: "http://localhost:1234/",
                scope: consts::LOGIN_SCOPE,
                state: "st",
                code_challenge: &pkce.challenge,
                login_hint: Some("user@example.com"),
                prompt: Some("select_account"),
                claims: None,
            },
        );
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(parsed.path(), "/organizations/oauth2/v2.0/authorize");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], consts::CLIENT_ID);
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["redirect_uri"], "http://localhost:1234/");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["login_hint"], "user@example.com");
        assert_eq!(pairs["prompt"], "select_account");
        assert!(pairs["scope"].contains("offline_access"));
        assert!(!pairs.contains_key("claims"));
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        use sha2::{Digest, Sha256};
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = util::b64url_encode(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let fresh = Pkce::new();
        assert!(fresh.verifier.len() >= 43 && fresh.verifier.len() <= 128);
    }

    #[test]
    fn parses_redirect_url() {
        let r = parse_redirect("http://localhost:5000/?code=abc.def&state=xyz&session_state=1").unwrap();
        assert_eq!(r, RedirectResult { code: "abc.def".into(), state: Some("xyz".into()) });
    }

    #[test]
    fn parses_bare_query_and_bare_code() {
        assert_eq!(parse_redirect("?code=abc&state=s").unwrap().code, "abc");
        assert_eq!(parse_redirect("code=abc").unwrap().code, "abc");
        assert_eq!(parse_redirect("0.AAAA-bare").unwrap().code, "0.AAAA-bare");
    }

    #[test]
    fn redirect_errors() {
        let err = parse_redirect("http://localhost/?error=access_denied&error_subcode=cancel").unwrap_err();
        assert!(matches!(err, AuthError::Cancelled));

        let err = parse_redirect(
            "http://localhost/?error=invalid_request&error_description=AADSTS50011%3A+bad+redirect",
        )
        .unwrap_err();
        match err {
            AuthError::OAuth(e) => {
                assert_eq!(e.error, "invalid_request");
                assert_eq!(e.aadsts().as_deref(), Some("AADSTS50011"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(parse_redirect("http://localhost/").is_err());
        assert!(parse_redirect("").is_err());
    }

    #[test]
    fn token_body_parsing() {
        let ok = parse_token_body(
            200,
            r#"{"token_type":"Bearer","scope":"a b","expires_in":3599,"access_token":"AT","refresh_token":"RT","id_token":"ID"}"#,
        )
        .unwrap();
        assert_eq!(ok.access_token, "AT");
        assert_eq!(ok.expires_in, 3599);
        assert_eq!(ok.refresh_token.as_deref(), Some("RT"));

        // v1-style string expiry.
        let ok = parse_token_body(200, r#"{"access_token":"AT","expires_in":"3600"}"#).unwrap();
        assert_eq!(ok.expires_in, 3600);

        let err = parse_token_body(
            400,
            r#"{"error":"invalid_grant","error_description":"AADSTS70043: The refresh token has expired.\r\nTrace ID: x","error_codes":[70043],"claims":"{\"access_token\":{}}"}"#,
        )
        .unwrap_err();
        assert!(err.needs_interaction());
        assert_eq!(err.claims(), Some(r#"{"access_token":{}}"#));
        assert_eq!(err.to_string(), "AADSTS70043: The refresh token has expired.");

        assert!(matches!(parse_token_body(502, "<html>"), Err(AuthError::Other(_))));
    }

    #[test]
    fn policy_blocks_do_not_ask_for_interaction() {
        let err = OAuthError {
            error: "invalid_grant".into(),
            error_description: "AADSTS53003: Access has been blocked by Conditional Access policies.".into(),
            error_codes: vec![53003],
            ..Default::default()
        };
        assert!(!err.needs_interaction());
    }
}
