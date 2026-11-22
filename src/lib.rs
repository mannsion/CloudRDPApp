//! CloudRDPApp: a native Linux client for Azure Virtual Desktop and Windows 365.
//!
//! Signs in with Microsoft Entra ID, lists the desktops and apps from the
//! workspace feed, and connects to them with FreeRDP.

pub mod auth;
pub mod backend;
pub mod config;
pub mod consts;
pub mod feed;
pub mod helper;
pub mod paths;
pub mod rdp;
pub mod relay;
pub mod ui;
pub mod url_handler;
pub mod util;

/// Install the TLS crypto provider used by every HTTPS client and server in
/// this process. Call once at start-up.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The HTTP client shared by sign-in and feed requests.
pub fn http_client() -> reqwest::Client {
    install_crypto_provider();
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .expect("the HTTP client could not be initialised")
}
