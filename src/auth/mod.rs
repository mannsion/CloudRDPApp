//! Microsoft Entra ID sign-in for the Azure Virtual Desktop public client.
//!
//! Sign-in is the standard OAuth 2.0 authorization-code flow with PKCE, run
//! in the user's own browser on the client's registered broker redirect
//! (`ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>`), which the
//! browser hands to this program through the desktop's URL scheme handler
//! ([`callback`]). The refresh token is kept in the desktop keyring; access tokens for the feed,
//! the connection gateway and the session host are minted from it silently.

pub mod account;
pub mod callback;
pub mod manager;
pub mod oauth;
pub mod store;

pub use account::Account;
pub use manager::{AuthManager, PendingLogin};
pub use oauth::{OAuthClient, OAuthError, TokenResponse};

/// Errors raised while signing in or fetching tokens.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Entra ID rejected the request.
    #[error("{0}")]
    OAuth(#[from] OAuthError),
    /// Entra ID could not be reached.
    #[error("network error: {0}")]
    Network(String),
    /// No account is signed in, or its saved sign-in is gone.
    #[error("not signed in")]
    NotSignedIn,
    /// The user did not finish signing in.
    #[error("sign-in was cancelled")]
    Cancelled,
    #[error("timed out waiting for sign-in")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

impl AuthError {
    /// True when the only way forward is an interactive sign-in.
    pub fn needs_interaction(&self) -> bool {
        match self {
            AuthError::OAuth(err) => err.needs_interaction(),
            AuthError::NotSignedIn => true,
            _ => false,
        }
    }

    /// Claims challenge to pass to the next interactive sign-in, if Entra
    /// supplied one (Conditional Access step-up).
    pub fn claims(&self) -> Option<&str> {
        match self {
            AuthError::OAuth(err) => err.claims.as_deref(),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for AuthError {
    fn from(err: reqwest::Error) -> Self {
        AuthError::Network(crate::util::redact(&err.to_string()))
    }
}
