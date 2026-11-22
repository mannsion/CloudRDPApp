//! Workspace feed discovery: the list of desktops and apps an account is
//! entitled to, as served to Microsoft's own Remote Desktop clients.
//!
//! ```text
//! GET /api/arm/feeddiscovery -> <TenantFeedURLs><TenantFeedURL FeedURL=… WorkspaceName=…/>…
//! GET <FeedURL>              -> <ResourceCollection><Publisher><Resources><Resource …>
//!                                 <Icons>…  <HostingTerminalServers>…<ResourceFile URL=…/>
//! GET <ResourceFile URL>     -> the .rdp connection file for that resource
//! ```

pub mod client;
pub mod model;
pub mod parse;

pub use client::FeedClient;
pub use model::{Feed, Resource, ResourceKind, Workspace};

#[derive(Debug, Clone, thiserror::Error)]
pub enum FeedError {
    /// The access token was rejected; refresh it and retry.
    #[error("the service rejected the sign-in (HTTP 401)")]
    Unauthorized,
    #[error("access denied (HTTP 403): {0}")]
    Forbidden(String),
    #[error("the service answered HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("the feed could not be read: {0}")]
    Malformed(String),
    #[error("refusing to send the sign-in token to {0}")]
    Untrusted(String),
    #[error("the service listed no desktops or apps for this account. If you expect some, press Retry: the list is sometimes empty for a moment after signing in")]
    Empty,
    #[error("this resource has no connection file")]
    NoRdpFile,
}
