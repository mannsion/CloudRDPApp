//! Protocol constants for Azure Virtual Desktop / Windows 365.
//!
//! Every value here is either advertised by Microsoft's own services or is the
//! default compiled into FreeRDP (`libfreerdp/core/settings.c`):
//!
//! * The feed service answers unauthenticated requests with
//!   `WWW-Authenticate: MS-WARA-CLAIMS hint="…;Client=a85cf173-…;Resource=https://www.wvd.microsoft.com…"`.
//! * FreeRDP ships the same client id as `FreeRDP_GatewayAvdClientID`.

/// Reverse-DNS application id (GApplication id, desktop file name).
pub const APP_ID: &str = "io.github.mannsion.CloudRDPApp";
/// Human readable name.
pub const APP_NAME: &str = "CloudRDPApp";
/// Directory name used below the XDG base directories.
pub const APP_DIR: &str = "cloudrdpapp";

/// Public client id of Microsoft's Remote Desktop / Windows App client.
pub const CLIENT_ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";

/// Entra ID authority host for the commercial cloud.
pub const AUTHORITY_HOST: &str = "login.microsoftonline.com";

/// Tenant path used for sign-in. `organizations` accepts any work account.
pub const DEFAULT_TENANT: &str = "organizations";

/// Resource scope of the AVD service (feed and connection gateway).
pub const WVD_SCOPE: &str = "https://www.wvd.microsoft.com/.default";

/// Scopes requested at sign-in.
pub const LOGIN_SCOPE: &str = "https://www.wvd.microsoft.com/.default offline_access openid profile";

/// Marker that identifies a session-host (RDS AAD) scope requested by FreeRDP.
pub const SESSION_HOST_SCOPE_PREFIX: &str = "ms-device-service://termsrv.wvd.microsoft.com/";

/// Feed discovery endpoint ("subscribe URL" of the official clients).
pub const FEED_DISCOVERY_URL: &str = "https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery";

/// Out-of-band redirect registered for the public client. Used for the
/// "paste the address" fallback sign-in.
pub const NATIVE_CLIENT_REDIRECT: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";

/// The feed service only answers clients that send an approved
/// `X-MS-User-Agent`. This is the value Microsoft's HTML5 client sends.
/// Override with `CLOUDRDPAPP_FEED_USER_AGENT` if the service ever changes.
pub const FEED_USER_AGENT: &str = "com.microsoft.rdc.html/2.0.79.2 rdhtml-sdk/2.0.4";

pub const ACCEPT_DISCOVERY: &str = "application/x-msts-radc-discovery+xml,text/xml";
pub const ACCEPT_FEED: &str = "application/x-msts-radc+xml;radc_schema_version=2.0,text/xml";

/// Hosts a bearer token may be sent to. Feed XML supplies further URLs
/// (tenant feeds, .rdp files, icons); they are checked against this list
/// before the `Authorization` header is attached.
pub const TRUSTED_HOST_SUFFIXES: &[&str] = &[".microsoft.com"];

/// First FreeRDP release with the out-of-process sign-in helper protocol
/// (`/azure:auth-helper:`), which the silent connection sign-in relies on.
pub const MIN_FREERDP_VERSION: (u32, u32, u32) = (3, 32, 0);

/// Environment variables that tie a FreeRDP child (and the helper it spawns)
/// back to the running application.
pub const ENV_HELPER_SOCKET: &str = "CLOUDRDPAPP_HELPER_SOCKET";
pub const ENV_HELPER_SESSION: &str = "CLOUDRDPAPP_HELPER_SESSION";
pub const ENV_HELPER_SECRET: &str = "CLOUDRDPAPP_HELPER_SECRET";

pub fn feed_user_agent() -> String {
    std::env::var("CLOUDRDPAPP_FEED_USER_AGENT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| FEED_USER_AGENT.to_owned())
}

/// True only for an https URL on a Microsoft host. The match is on a dot
/// boundary, so `evilmicrosoft.com` and `microsoft.com.evil.io` are rejected.
pub fn is_trusted_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    TRUSTED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == suffix[1..] || host.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_urls() {
        assert!(is_trusted_url("https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery"));
        assert!(is_trusted_url("https://microsoft.com/x"));
        assert!(is_trusted_url("https://RDWEB-G-US-R1.WVD.MICROSOFT.COM/api"));
    }

    #[test]
    fn untrusted_urls() {
        assert!(!is_trusted_url("http://rdweb.wvd.microsoft.com/api"));
        assert!(!is_trusted_url("https://evilmicrosoft.com/"));
        assert!(!is_trusted_url("https://microsoft.com.evil.io/"));
        assert!(!is_trusted_url("https://rdweb.wvd.microsoft.com.evil.io/"));
        assert!(!is_trusted_url("not a url"));
        assert!(!is_trusted_url("file:///etc/passwd"));
    }
}
