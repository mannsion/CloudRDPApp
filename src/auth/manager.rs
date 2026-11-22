//! Holds the signed-in account and hands out access tokens.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::callback::{self, CallbackHub, CallbackWaiter};
use super::oauth::{self, AuthorizeParams, OAuthClient, Pkce, TokenResponse};
use super::store::{KIND_REFRESH_TOKEN, SecretStore};
use super::{Account, AuthError};
use crate::consts;
use crate::util;

/// Tokens are refreshed when they have less than this left.
const MIN_REMAINING: Duration = Duration::from_secs(5 * 60);
/// A cached gateway token handed to FreeRDP must outlive connection setup.
const MIN_REMAINING_FOR_RELAY: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<Inner>,
}

struct Inner {
    oauth: OAuthClient,
    store: SecretStore,
    callbacks: CallbackHub,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    account: Option<Account>,
    refresh_token: Option<String>,
    tokens: HashMap<String, CachedToken>,
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// An interactive sign-in that has been prepared but not completed.
pub struct PendingLogin {
    /// The address to open in the browser.
    pub url: String,
    /// The broker callback this sign-in waits for; `None` for the paste fallback.
    waiter: Option<CallbackWaiter>,
    manual: bool,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
}

impl PendingLogin {
    /// True for the paste-the-address fallback.
    pub fn is_manual(&self) -> bool {
        self.manual
    }

    /// The redirect URI this sign-in was started with.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The `state` sent to Entra ID (and expected back).
    pub fn state(&self) -> &str {
        &self.state
    }
}

/// Options for an interactive sign-in.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    /// Pre-select this account in the browser.
    pub login_hint: Option<String>,
    /// Conditional Access claims challenge from a failed token request.
    pub claims: Option<String>,
    /// `select_account`, `login`, or none to let single sign-on complete silently.
    pub prompt: Option<String>,
}

impl AuthManager {
    pub fn new(oauth: OAuthClient, store: SecretStore) -> Self {
        Self {
            inner: Arc::new(Inner { oauth, store, callbacks: CallbackHub::new(), state: Mutex::new(State::default()) }),
        }
    }

    /// Where `--auth-callback` deliveries are routed to waiting sign-ins.
    pub fn callbacks(&self) -> &CallbackHub {
        &self.inner.callbacks
    }

    /// The broker redirect URI of this client.
    pub fn broker_redirect_uri(&self) -> String {
        callback::broker_redirect_uri(self.inner.oauth.client_id())
    }

    pub fn oauth(&self) -> &OAuthClient {
        &self.inner.oauth
    }

    /// Where secrets are stored, for display.
    pub fn storage_description(&self) -> &'static str {
        self.inner.store.description()
    }

    pub fn store(&self) -> &SecretStore {
        &self.inner.store
    }

    pub async fn account(&self) -> Option<Account> {
        self.inner.state.lock().await.account.clone()
    }

    /// Load the saved sign-in of `account`. Returns false when there is none.
    pub async fn restore(&self, account: &Account) -> bool {
        let Some(refresh_token) = self.inner.store.get(&account.id(), KIND_REFRESH_TOKEN).await else {
            return false;
        };
        let mut state = self.inner.state.lock().await;
        state.account = Some(account.clone());
        state.refresh_token = Some(refresh_token);
        state.tokens.clear();
        true
    }

    /// Prepare a browser sign-in on the broker redirect. The browser hands
    /// the final address to `cloudrdpapp --auth-callback`, which delivers it
    /// to [`callbacks`](Self::callbacks).
    pub fn start_login(&self, options: &LoginOptions) -> PendingLogin {
        self.prepare(options, self.broker_redirect_uri(), false)
    }

    /// Prepare the fallback sign-in where the user pastes the final address.
    pub fn start_manual_login(&self, options: &LoginOptions) -> PendingLogin {
        self.prepare(options, consts::NATIVE_CLIENT_REDIRECT.to_owned(), true)
    }

    fn prepare(&self, options: &LoginOptions, redirect_uri: String, manual: bool) -> PendingLogin {
        let pkce = Pkce::new();
        let state = util::random_token(24);
        let waiter = (!manual).then(|| self.inner.callbacks.expect(&state));
        let url = self.inner.oauth.authorize_url(
            consts::DEFAULT_TENANT,
            &AuthorizeParams {
                redirect_uri: &redirect_uri,
                scope: consts::LOGIN_SCOPE,
                state: &state,
                code_challenge: &pkce.challenge,
                login_hint: options.login_hint.as_deref(),
                prompt: options.prompt.as_deref(),
                claims: options.claims.as_deref(),
            },
        );
        PendingLogin { url, waiter, manual, pkce, state, redirect_uri }
    }

    /// Wait for the browser to come back, then finish signing in.
    ///
    /// `expected` pins the account: a re-authentication that ends with a
    /// different user is rejected instead of silently switching accounts.
    pub async fn finish_login(
        &self,
        mut pending: PendingLogin,
        timeout: Duration,
        expected: Option<&Account>,
    ) -> Result<Account, AuthError> {
        let waiter = pending
            .waiter
            .take()
            .ok_or_else(|| AuthError::Other("this sign-in expects a pasted address".to_owned()))?;
        let landed = waiter.wait(timeout).await?;
        self.complete(&pending, &landed, expected).await
    }

    /// Finish the paste-the-address fallback.
    pub async fn finish_manual_login(
        &self,
        pending: &PendingLogin,
        pasted: &str,
        expected: Option<&Account>,
    ) -> Result<Account, AuthError> {
        self.complete(pending, pasted, expected).await
    }

    async fn complete(
        &self,
        pending: &PendingLogin,
        landed: &str,
        expected: Option<&Account>,
    ) -> Result<Account, AuthError> {
        let redirect = oauth::parse_redirect(landed)?;
        // A bare pasted code carries no state; anything else must echo ours.
        if let Some(state) = &redirect.state {
            if state != &pending.state {
                return Err(AuthError::Other(
                    "the sign-in response does not belong to this request; please try again".to_owned(),
                ));
            }
        } else if !pending.is_manual() {
            return Err(AuthError::Other("the sign-in response is missing its state".to_owned()));
        }

        let tokens = self
            .inner
            .oauth
            .exchange_code(
                consts::DEFAULT_TENANT,
                &redirect.code,
                &pending.redirect_uri,
                &pending.pkce.verifier,
                consts::LOGIN_SCOPE,
            )
            .await?;
        let id_token = tokens
            .id_token
            .as_deref()
            .ok_or_else(|| AuthError::Other("Entra ID did not return an ID token".to_owned()))?;
        let account = Account::from_id_token(id_token)?;
        if let Some(expected) = expected
            && expected.id() != account.id()
        {
            return Err(AuthError::Other(format!(
                "signed in as {} but this session belongs to {}",
                account.username, expected.username
            )));
        }
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| AuthError::Other("Entra ID did not return a refresh token".to_owned()))?;

        self.persist_refresh_token(&account, &refresh_token).await;
        let mut state = self.inner.state.lock().await;
        state.account = Some(account.clone());
        state.refresh_token = Some(refresh_token);
        state.tokens.clear();
        cache_token(&mut state, consts::WVD_SCOPE, &tokens);
        Ok(account)
    }

    async fn persist_refresh_token(&self, account: &Account, refresh_token: &str) {
        if let Err(err) = self.inner.store.set(&account.id(), KIND_REFRESH_TOKEN, refresh_token).await {
            tracing::warn!("could not save the sign-in to {}: {err}", self.inner.store.description());
        }
    }

    /// An access token for `scope`, refreshed silently when needed.
    pub async fn access_token(&self, scope: &str) -> Result<String, AuthError> {
        let key = cache_key(scope);
        let mut state = self.inner.state.lock().await;
        if let Some(token) = state.tokens.get(&key)
            && token.expires_at > Instant::now() + MIN_REMAINING
        {
            return Ok(token.access_token.clone());
        }
        let tokens = self.refresh_locked(&mut state, scope, &[]).await?;
        Ok(tokens.access_token)
    }

    /// Forget cached access tokens (after a 401 from the service).
    pub async fn invalidate(&self, scope: &str) {
        self.inner.state.lock().await.tokens.remove(&cache_key(scope));
    }

    /// Check that a token for `scope` can be obtained without user
    /// interaction. Used before FreeRDP asks for it, so an expired sign-in
    /// can be repaired in the browser first.
    pub async fn preflight(&self, scope: &str) -> Result<(), AuthError> {
        let mut state = self.inner.state.lock().await;
        self.refresh_locked(&mut state, scope, &[]).await.map(|_| ())
    }

    /// Produce a token endpoint response for FreeRDP: the raw status and JSON
    /// body, with `req_cnf` forwarded so the token is bound to FreeRDP's
    /// proof-of-possession key. Refresh and ID tokens are stripped.
    pub async fn token_response(&self, scope: &str, req_cnf: Option<&str>) -> Result<(u16, String), AuthError> {
        let key = cache_key(scope);
        let mut state = self.inner.state.lock().await;

        if req_cnf.is_none()
            && let Some(token) = state.tokens.get(&key)
        {
            let now = Instant::now();
            if token.expires_at > now + MIN_REMAINING_FOR_RELAY {
                let body = serde_json::json!({
                    "token_type": "Bearer",
                    "expires_in": (token.expires_at - now).as_secs(),
                    "access_token": token.access_token,
                });
                return Ok((200, body.to_string()));
            }
        }

        let (account, refresh_token) = signed_in(&state)?;
        let extra: Vec<(&str, &str)> = req_cnf.map(|cnf| vec![("req_cnf", cnf)]).unwrap_or_default();
        let (status, body) = self
            .inner
            .oauth
            .refresh_raw(&account.tenant_id, &refresh_token, scope, &extra)
            .await?;
        if !(200..300).contains(&status) {
            // Relay Entra's error unchanged so FreeRDP logs the real reason.
            return Ok((status, body));
        }

        let mut json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|err| AuthError::Other(format!("unexpected token response: {err}")))?;
        if let Some(rotated) = json.get("refresh_token").and_then(|v| v.as_str()).map(str::to_owned)
            && state.refresh_token.as_deref() != Some(rotated.as_str())
        {
            state.refresh_token = Some(rotated.clone());
            self.persist_refresh_token(&account, &rotated).await;
        }
        if req_cnf.is_none()
            && let Ok(tokens) = serde_json::from_value::<TokenResponse>(json.clone())
        {
            cache_token(&mut state, scope, &tokens);
        }
        if let Some(object) = json.as_object_mut() {
            object.remove("refresh_token");
            object.remove("id_token");
            object.remove("client_info");
        }
        Ok((status, json.to_string()))
    }

    async fn refresh_locked(
        &self,
        state: &mut State,
        scope: &str,
        extra: &[(&str, &str)],
    ) -> Result<TokenResponse, AuthError> {
        let (account, refresh_token) = signed_in(state)?;
        let tokens = self.inner.oauth.refresh(&account.tenant_id, &refresh_token, scope, extra).await?;
        if let Some(rotated) = &tokens.refresh_token
            && state.refresh_token.as_deref() != Some(rotated.as_str())
        {
            state.refresh_token = Some(rotated.clone());
            self.persist_refresh_token(&account, rotated).await;
        }
        if extra.is_empty() {
            cache_token(state, scope, &tokens);
        }
        Ok(tokens)
    }

    /// Forget the account and remove its saved sign-in.
    pub async fn sign_out(&self) {
        let mut state = self.inner.state.lock().await;
        if let Some(account) = state.account.take() {
            let id = account.id();
            self.inner.store.delete(&id, KIND_REFRESH_TOKEN).await;
            self.inner.store.delete_passwords_for_account(&id).await;
        }
        state.refresh_token = None;
        state.tokens.clear();
    }

    /// Install a signed-in state directly (tests and demo mode).
    pub async fn set_session(&self, account: Account, refresh_token: &str) {
        let mut state = self.inner.state.lock().await;
        state.account = Some(account);
        state.refresh_token = Some(refresh_token.to_owned());
        state.tokens.clear();
    }
}

fn signed_in(state: &State) -> Result<(Account, String), AuthError> {
    match (&state.account, &state.refresh_token) {
        (Some(account), Some(token)) => Ok((account.clone(), token.clone())),
        _ => Err(AuthError::NotSignedIn),
    }
}

fn cache_token(state: &mut State, scope: &str, tokens: &TokenResponse) {
    if tokens.access_token.is_empty() || tokens.expires_in == 0 {
        return;
    }
    state.tokens.insert(
        cache_key(scope),
        CachedToken {
            access_token: tokens.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(tokens.expires_in),
        },
    );
}

/// Tokens are cached per resource: the OIDC scopes FreeRDP adds to its
/// gateway request (`openid profile offline_access`) do not change the
/// resulting access token.
pub fn cache_key(scope: &str) -> String {
    const IDENTITY_SCOPES: &[&str] = &["openid", "profile", "offline_access", "email"];
    let mut parts: Vec<&str> = scope.split_whitespace().filter(|part| !IDENTITY_SCOPES.contains(part)).collect();
    parts.sort_unstable();
    parts.dedup();
    parts.join(" ")
}

/// True when `scope` asks for the AVD service (feed or gateway).
pub fn is_gateway_scope(scope: &str) -> bool {
    scope.split_whitespace().any(|part| part == consts::WVD_SCOPE)
}

/// True when `scope` asks for a session host (RDS AAD) token.
pub fn is_session_host_scope(scope: &str) -> bool {
    scope.split_whitespace().any(|part| part.starts_with(consts::SESSION_HOST_SCOPE_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_ignore_identity_scopes() {
        assert_eq!(cache_key(consts::LOGIN_SCOPE), consts::WVD_SCOPE);
        assert_eq!(
            cache_key("https://www.wvd.microsoft.com/.default openid profile offline_access"),
            consts::WVD_SCOPE
        );
        assert_eq!(cache_key("b a"), "a b");
    }

    #[test]
    fn scope_classification() {
        assert!(is_gateway_scope("https://www.wvd.microsoft.com/.default openid profile offline_access"));
        assert!(!is_gateway_scope("ms-device-service://termsrv.wvd.microsoft.com/name/host/user_impersonation"));
        assert!(is_session_host_scope("ms-device-service://termsrv.wvd.microsoft.com/name/host/user_impersonation"));
        assert!(!is_session_host_scope(consts::WVD_SCOPE));
    }

    #[tokio::test]
    async fn requires_sign_in() {
        let manager = AuthManager::new(OAuthClient::new(crate::http_client()), SecretStore::in_memory());
        assert!(manager.account().await.is_none());
        let err = manager.access_token(consts::WVD_SCOPE).await.unwrap_err();
        assert!(matches!(err, AuthError::NotSignedIn));
        assert!(err.needs_interaction());
    }

    #[tokio::test]
    async fn restore_uses_the_store() {
        let store = SecretStore::in_memory();
        let account = Account {
            tenant_id: "t".into(),
            object_id: "o".into(),
            username: "u@x.io".into(),
            name: None,
        };
        let manager = AuthManager::new(OAuthClient::new(crate::http_client()), store);
        assert!(!manager.restore(&account).await);
        manager.store().set(&account.id(), KIND_REFRESH_TOKEN, "rt").await.unwrap();
        assert!(manager.restore(&account).await);
        assert_eq!(manager.account().await, Some(account.clone()));
        manager.sign_out().await;
        assert!(manager.account().await.is_none());
        assert_eq!(manager.store().get(&account.id(), KIND_REFRESH_TOKEN).await, None);
    }

    #[tokio::test]
    async fn manual_login_url_uses_the_native_client_redirect() {
        let manager = AuthManager::new(OAuthClient::new(crate::http_client()), SecretStore::in_memory());
        let pending = manager.start_manual_login(&LoginOptions {
            login_hint: Some("u@x.io".into()),
            ..Default::default()
        });
        assert!(pending.is_manual());
        let url = url::Url::parse(&pending.url).unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["redirect_uri"], consts::NATIVE_CLIENT_REDIRECT);
        assert_eq!(pairs["login_hint"], "u@x.io");
        // A response carrying someone else's state is rejected before any network call.
        let err = manager
            .finish_manual_login(&pending, "https://x/?code=abc&state=not-ours", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not belong"));
    }

    #[tokio::test]
    async fn browser_login_uses_the_broker_redirect_and_its_callback() {
        let manager = AuthManager::new(OAuthClient::new(crate::http_client()), SecretStore::in_memory());
        let pending = manager.start_login(&LoginOptions::default());
        assert!(!pending.is_manual());
        let url = url::Url::parse(&pending.url).unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["redirect_uri"], format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{}", consts::CLIENT_ID));
        assert_eq!(pending.redirect_uri(), pairs["redirect_uri"]);
        assert_eq!(pairs["state"], pending.state());
        assert_eq!(manager.callbacks().pending(), 1);

        // The user cancels in the browser: the callback with our state ends
        // the wait with Cancelled, before any token request.
        let callback = format!(
            "ms-appx-web://Microsoft.AAD.BrokerPlugin/{}?error=access_denied&error_subcode=cancel&state={}",
            consts::CLIENT_ID,
            pending.state()
        );
        manager.callbacks().deliver(&callback).unwrap();
        let err = manager.finish_login(pending, Duration::from_secs(2), None).await.unwrap_err();
        assert!(matches!(err, AuthError::Cancelled), "{err}");
        assert_eq!(manager.callbacks().pending(), 0);

        // Abandoning a sign-in (the UI's Cancel drops it) stops waiting.
        let pending = manager.start_login(&LoginOptions::default());
        assert_eq!(manager.callbacks().pending(), 1);
        drop(pending);
        assert_eq!(manager.callbacks().pending(), 0);
    }
}
