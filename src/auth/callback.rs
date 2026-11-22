//! Browser sign-in on the broker redirect.
//!
//! The public client has no loopback redirect registered; it does have
//! `ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>` (FreeRDP's default
//! `avd-token` redirect). After sign-in the browser hands that address to the
//! desktop's handler for the `ms-appx-web` scheme, which is this program
//! started as `cloudrdpapp --auth-callback <url>` (see `crate::url_handler`).
//! That process forwards the address over the app's helper socket, and the
//! [`CallbackHub`] hands it to the sign-in waiting for its `state`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::AuthError;

/// URL scheme of the broker redirect.
pub const SCHEME: &str = "ms-appx-web";
/// Every callback must start with this (compared ignoring ASCII case).
pub const CALLBACK_PREFIX: &str = "ms-appx-web://microsoft.aad.brokerplugin/";
/// Longest callback accepted. Codes are a few kilobytes; error descriptions
/// and claims can add some more.
pub const MAX_CALLBACK_LEN: usize = 32 * 1024;

/// The broker redirect URI for `client_id`, exactly as registered.
pub fn broker_redirect_uri(client_id: &str) -> String {
    format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{client_id}")
}

/// True when `uri` is a broker redirect (any client id, any case).
pub fn is_broker_redirect(uri: &str) -> bool {
    uri.get(..CALLBACK_PREFIX.len()).is_some_and(|head| head.eq_ignore_ascii_case(CALLBACK_PREFIX))
}

/// Check an address handed to `--auth-callback` before it goes anywhere.
pub fn validate_callback_url(url: &str) -> Result<(), String> {
    if url.len() > MAX_CALLBACK_LEN {
        return Err(format!("the sign-in address is longer than {MAX_CALLBACK_LEN} bytes"));
    }
    if url.chars().any(char::is_control) {
        return Err("the sign-in address contains control characters".to_owned());
    }
    if !is_broker_redirect(url) {
        return Err(format!("the address is not a sign-in response (it must start with {CALLBACK_PREFIX})"));
    }
    Ok(())
}

/// The `state` parameter of a callback (query, or fragment if there is no query).
pub fn callback_state(url: &str) -> Option<String> {
    let rest = url.split_once('?').map(|(_, q)| q).or_else(|| url.split_once('#').map(|(_, f)| f))?;
    let query = rest.split('#').next().unwrap_or("");
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|state| !state.is_empty())
}

/// Routes callbacks to the sign-ins waiting for them, keyed by `state`.
#[derive(Clone, Default)]
pub struct CallbackHub {
    waiting: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl CallbackHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait for the callback carrying `state`. A second waiter for the same
    /// state replaces the first (whose wait then fails).
    pub fn expect(&self, state: &str) -> CallbackWaiter {
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(state.to_owned(), tx);
        CallbackWaiter { hub: self.clone(), state: state.to_owned(), rx: Some(rx) }
    }

    /// Hand a callback to its sign-in. Fails when it is malformed or no
    /// sign-in waits for its state (finished, cancelled or someone else's).
    pub fn deliver(&self, url: &str) -> Result<(), String> {
        validate_callback_url(url)?;
        let state = callback_state(url).ok_or("the sign-in response carries no state")?;
        let sender = self
            .waiting
            .lock()
            .unwrap()
            .remove(&state)
            .ok_or_else(|| format!("no sign-in in this {} window is waiting for this response", crate::consts::APP_NAME))?;
        sender.send(url.to_owned()).map_err(|_| "the sign-in was cancelled".to_owned())
    }

    /// Number of sign-ins waiting.
    pub fn pending(&self) -> usize {
        self.waiting.lock().unwrap().len()
    }

    fn forget(&self, state: &str) {
        let mut waiting = self.waiting.lock().unwrap();
        // Only our own (now closed) entry, not a live one that replaced it.
        if waiting.get(state).is_some_and(|tx| tx.is_closed()) {
            waiting.remove(state);
        }
    }
}

/// One sign-in's claim on its callback. Dropping it stops waiting.
pub struct CallbackWaiter {
    hub: CallbackHub,
    state: String,
    rx: Option<oneshot::Receiver<String>>,
}

impl std::fmt::Debug for CallbackWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackWaiter").finish_non_exhaustive()
    }
}

impl CallbackWaiter {
    /// Wait up to `timeout` for the callback and return its full address.
    pub async fn wait(mut self, timeout: Duration) -> Result<String, AuthError> {
        let rx = self.rx.take().expect("waited once");
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(url)) => Ok(url),
            Ok(Err(_)) => Err(AuthError::Other("another sign-in replaced this one".to_owned())),
            Err(_) => Err(AuthError::Timeout),
        }
    }
}

impl Drop for CallbackWaiter {
    fn drop(&mut self) {
        // Dropping the receiver closes the sender kept in the hub.
        drop(self.rx.take());
        self.hub.forget(&self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";

    #[test]
    fn broker_redirect_matches_freerdps_default() {
        // FreeRDP_GatewayAvdAccessTokenFormat, unescaped, with the client id.
        assert_eq!(broker_redirect_uri(ID), format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{ID}"));
        assert!(is_broker_redirect(&broker_redirect_uri(ID)));
    }

    #[test]
    fn validation_accepts_broker_callbacks_in_any_case() {
        assert!(validate_callback_url(&format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{ID}?code=abc&state=s")).is_ok());
        assert!(validate_callback_url(&format!("MS-APPX-WEB://MICROSOFT.AAD.BROKERPLUGIN/{ID}?code=abc")).is_ok());
        assert!(validate_callback_url("ms-appx-web://microsoft.aad.brokerplugin/x?error=access_denied").is_ok());
    }

    #[test]
    fn validation_rejects_other_addresses() {
        for bad in [
            "",
            "ms-appx-web://",
            "ms-appx-web://Microsoft.AAD.BrokerPlugin",
            "ms-appx-web://Microsoft.AAD.BrokerPluginX/?code=1",
            "ms-appx-web://evil.example/?code=1&state=s",
            "https://login.microsoftonline.com/common/oauth2/nativeclient?code=1",
            "http://localhost:1234/?code=1",
            " ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1",
            "ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1\nstate=s",
            "ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1\u{7f}",
            "ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1\u{85}",
            "ms-appx-wéb://Microsoft.AAD.BrokerPlugin/x",
        ] {
            assert!(validate_callback_url(bad).is_err(), "{bad:?} was accepted");
        }
        let long = format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code={}", "a".repeat(MAX_CALLBACK_LEN));
        assert!(validate_callback_url(&long).unwrap_err().contains("longer"));
    }

    #[test]
    fn state_extraction() {
        let base = "ms-appx-web://Microsoft.AAD.BrokerPlugin/x";
        assert_eq!(callback_state(&format!("{base}?code=1&state=a%2Bb")).as_deref(), Some("a+b"));
        assert_eq!(callback_state(&format!("{base}?state=first&state=second")).as_deref(), Some("first"));
        assert_eq!(callback_state(&format!("{base}#code=1&state=frag")).as_deref(), Some("frag"));
        assert_eq!(callback_state(&format!("{base}?code=1&state=q#x")).as_deref(), Some("q"));
        assert_eq!(callback_state(&format!("{base}?code=1")), None);
        assert_eq!(callback_state(&format!("{base}?code=1&state=")), None);
        assert_eq!(callback_state(base), None);
    }

    #[tokio::test]
    async fn callbacks_reach_the_sign_in_with_their_state() {
        let hub = CallbackHub::new();
        let first = hub.expect("state-1");
        let second = hub.expect("state-2");
        assert_eq!(hub.pending(), 2);

        let url2 = format!("ms-appx-web://Microsoft.AAD.BrokerPlugin/{ID}?code=two&state=state-2");
        hub.deliver(&url2).unwrap();
        assert_eq!(second.wait(Duration::from_secs(1)).await.unwrap(), url2);

        // Unknown state, missing state, foreign address: refused, nothing consumed.
        let err = hub.deliver("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1&state=other").unwrap_err();
        assert!(err.contains("no sign-in"), "{err}");
        assert!(hub.deliver("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1").unwrap_err().contains("no state"));
        assert!(hub.deliver("https://x/?code=1&state=state-1").is_err());
        assert_eq!(hub.pending(), 1);

        // A delivered state cannot be replayed.
        assert!(hub.deliver(&url2).is_err());

        // Dropping (cancelling) a waiter removes it.
        drop(first);
        assert_eq!(hub.pending(), 0);
        assert!(hub.deliver("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1&state=state-1").is_err());
    }

    #[tokio::test]
    async fn waits_time_out_and_clean_up() {
        let hub = CallbackHub::new();
        let waiter = hub.expect("s");
        let err = waiter.wait(Duration::from_millis(20)).await.unwrap_err();
        assert!(matches!(err, AuthError::Timeout));
        assert_eq!(hub.pending(), 0);
    }

    #[tokio::test]
    async fn a_replaced_waiter_does_not_remove_its_successor() {
        let hub = CallbackHub::new();
        let old = hub.expect("s");
        let new = hub.expect("s");
        drop(old);
        assert_eq!(hub.pending(), 1);
        hub.deliver("ms-appx-web://Microsoft.AAD.BrokerPlugin/x?code=1&state=s").unwrap();
        assert!(new.wait(Duration::from_secs(1)).await.is_ok());
    }
}
