//! FreeRDP's out-of-process sign-in helper, implemented by this executable.
//!
//! FreeRDP (3.32 and later) starts the program named by `/azure:auth-helper:`
//! with `--cmdInFd=…` and `--cmdOutFd=…` and speaks newline-delimited JSON-RPC
//! over those pipes (`client/common/aad-helper-protocol.md` in the FreeRDP
//! sources). A helper is asked to "navigate" to an authorize URL and report
//! the redirect it ends on.
//!
//! When started that way this program is a thin [`bridge`]: it forwards each
//! navigate request over a Unix socket to the running application
//! ([`server`]), which decides how to answer it.
//!
//! The same socket also takes broker sign-in responses from
//! `cloudrdpapp --auth-callback <url>` ([`callback`]).

pub mod bridge;
pub mod callback;
pub mod server;

use serde::{Deserialize, Serialize};

/// A navigate request forwarded from the bridge to the application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigateRequest {
    /// Session id the FreeRDP process was started for.
    pub session: String,
    /// Per-session secret proving the request comes from that process tree.
    pub secret: String,
    /// The authorize URL FreeRDP wants shown.
    pub url: String,
    /// Prefix of the address that completes the sign-in.
    pub redirect_uri: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub timeout_ms: u64,
}

/// The application's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigateResponse {
    /// The address the sign-in ended on, with `code` and `state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// `timeout`, `user_cancelled`, or a description of what went wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl NavigateResponse {
    pub fn ok(redirect_url: impl Into<String>) -> Self {
        Self { redirect_url: Some(redirect_url.into()), error: None }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { redirect_url: None, error: Some(message.into()) }
    }
}

/// A broker sign-in response forwarded by `cloudrdpapp --auth-callback`.
/// Any process of the same user can reach the socket; the response is only
/// used by the sign-in whose random `state` it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCallback {
    pub auth_callback: String,
}

/// The application's answer to an [`AuthCallback`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCallbackResponse {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Error strings with a defined meaning in FreeRDP's helper protocol.
pub const ERROR_TIMEOUT: &str = "timeout";
pub const ERROR_CANCELLED: &str = "user_cancelled";
