//! View decisions that do not need GTK: search, card states, which message
//! to show. Kept apart so they can be unit tested.

use crate::auth::AuthError;
use crate::backend::{BackendError, Clients};
use crate::config::ClientKind;
use crate::feed::Feed;
use crate::rdp::freerdp;

/// What a resource card shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CardState {
    #[default]
    Idle,
    Connecting,
    Connected,
    /// The connection failed or ended with an error; the reason in a sentence.
    Failed(String),
}

impl CardState {
    pub fn is_active(&self) -> bool {
        matches!(self, CardState::Connecting | CardState::Connected)
    }
}

/// True when every word of `query` appears in the title or the workspace
/// name (case-insensitive). An empty query matches everything.
pub fn matches_search(query: &str, title: &str, workspace: &str) -> bool {
    let title = title.to_lowercase();
    let workspace = workspace.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| title.contains(&word) || workspace.contains(&word))
}

/// How a FreeRDP process ended, as far as the GUI knows.
#[derive(Debug, Clone, Default)]
pub struct ExitInfo<'a> {
    /// The session had reached the remote desktop.
    pub connected: bool,
    /// The user pressed Disconnect (or the app stopped it).
    pub stopped_by_user: bool,
    pub code: Option<i32>,
    /// The last known failure found in the log (`freerdp::explain_log_line`).
    pub explanation: Option<&'a str>,
}

/// The card state after a session ended.
pub fn state_after_exit(exit: &ExitInfo<'_>) -> CardState {
    if exit.stopped_by_user {
        return CardState::Idle;
    }
    let normal = matches!(exit.code, Some(0 | 1 | 2 | 11));
    if normal && exit.connected {
        return CardState::Idle;
    }
    if let Some(explanation) = exit.explanation {
        return CardState::Failed(explanation.to_owned());
    }
    match exit.code {
        Some(code) => match freerdp::describe_exit(code) {
            Some(text) => CardState::Failed(text.to_owned()),
            None if normal => CardState::Idle,
            None => CardState::Failed(format!("FreeRDP ended with error code {code}. The log has the details.")),
        },
        None => CardState::Failed("FreeRDP stopped unexpectedly. The log has the details.".to_owned()),
    }
}

/// True when the failure points at a wrong password (so a remembered one
/// should be forgotten).
pub fn is_credential_failure(code: Option<i32>, explanation: Option<&str>) -> bool {
    if matches!(code, Some(132 | 134 | 154 | 159)) {
        return true;
    }
    explanation.is_some_and(|text| {
        text == freerdp::explain_log_line("ERRCONNECT_LOGON_FAILURE").unwrap_or_default()
            || text == freerdp::explain_log_line("ERRCONNECT_AUTHENTICATION_FAILED").unwrap_or_default()
    })
}

/// A sign-in error as a full sentence, with the `AADSTS…` code when Entra
/// supplied one.
pub fn sign_in_error_text(err: &BackendError) -> String {
    let mut text = error_text(err);
    if let BackendError::Auth(AuthError::OAuth(oauth)) = err
        && let Some(code) = oauth.aadsts()
        && !text.contains(&code)
    {
        text = format!("{} (error {code}).", text.trim_end_matches('.'));
    }
    text
}

/// Any backend error as one clean sentence. The core appends a full stop
/// to messages that may already end with one.
pub fn error_text(err: &BackendError) -> String {
    let mut text = err.to_string().trim().to_owned();
    while text.ends_with("..") && !text.ends_with("...") {
        text.pop();
    }
    text
}

/// The persistent warning to show when the configured FreeRDP client
/// cannot be used, or `None` when it is fine.
pub fn client_banner(clients: &Clients, kind: ClientKind) -> Option<String> {
    let problem = match clients.get(kind) {
        None => BackendError::ClientMissing(kind),
        Some(client) if !client.supports_helper() => {
            BackendError::ClientTooOld { kind, version: client.version_string() }
        }
        Some(_) => return None,
    };
    Some(format!("{problem} Install or update FreeRDP using your distribution's package manager."))
}

/// A monitor scale factor (1.45) as a percentage (145).
pub fn scale_percent(scale: f64) -> Option<u32> {
    (scale.is_finite() && scale > 0.0).then(|| (scale * 100.0).round() as u32)
}

/// Keyring key for a remembered password: account and resource together.
pub fn password_key(account_id: &str, resource_key: &str) -> String {
    format!("{account_id}/{resource_key}")
}

/// Which body the main view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedView {
    /// Nothing to show yet; the first download is running.
    Loading,
    /// The download failed and there is nothing cached.
    Failed,
    /// The account has no desktops or apps.
    Empty,
    /// The search matches nothing.
    NoMatches,
    Grid,
}

pub fn feed_view(feed: Option<&Feed>, loading: bool, query: &str) -> FeedView {
    match feed {
        None if loading => FeedView::Loading,
        None => FeedView::Failed,
        Some(feed) if feed.is_empty() => FeedView::Empty,
        Some(feed) if !feed.resources().any(|r| matches_search(query, &r.title, &r.workspace)) => FeedView::NoMatches,
        Some(_) => FeedView::Grid,
    }
}

/// The feed warnings as one short paragraph.
pub fn warnings_text(warnings: &[String]) -> Option<String> {
    match warnings {
        [] => None,
        [one] => Some(format!("A workspace could not be loaded: {}", ensure_sentence(one))),
        many => Some(format!(
            "{} workspaces could not be loaded. {}",
            many.len(),
            many.iter().map(|w| ensure_sentence(w)).collect::<Vec<_>>().join(" ")
        )),
    }
}

fn ensure_sentence(text: &str) -> String {
    let mut out = text.trim().to_owned();
    if let Some(first) = out.chars().next() {
        let upper: String = first.to_uppercase().collect();
        out.replace_range(..first.len_utf8(), &upper);
    }
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OAuthError;
    use crate::feed::{Resource, ResourceKind};
    use crate::rdp::freerdp::Client;

    #[test]
    fn search_matches_every_word_in_title_or_workspace() {
        assert!(matches_search("", "Finance Desktop", "Contoso"));
        assert!(matches_search("  ", "x", "y"));
        assert!(matches_search("fin", "Finance Desktop", "Contoso"));
        assert!(matches_search("FIN contoso", "Finance Desktop", "Contoso"));
        assert!(!matches_search("fin lab", "Finance Desktop", "Contoso"));
        assert!(matches_search("cloud", "My PC", "Windows 365 Cloud"));
    }

    #[test]
    fn exits_map_to_card_states() {
        let base = ExitInfo::default();
        assert_eq!(state_after_exit(&ExitInfo { stopped_by_user: true, code: Some(147), ..base.clone() }), CardState::Idle);
        assert_eq!(state_after_exit(&ExitInfo { connected: true, code: Some(1), ..base.clone() }), CardState::Idle);
        // A normal end before the desktop appeared (e.g. the certificate prompt was declined).
        assert_eq!(state_after_exit(&ExitInfo { code: Some(0), ..base.clone() }), CardState::Idle);
        // The log explanation wins over the exit code.
        let explained = ExitInfo { code: Some(131), explanation: Some("The gateway refused."), ..base.clone() };
        assert_eq!(state_after_exit(&explained), CardState::Failed("The gateway refused.".into()));
        assert_eq!(
            state_after_exit(&ExitInfo { code: Some(147), ..base.clone() }),
            CardState::Failed(freerdp::describe_exit(147).unwrap().into())
        );
        let CardState::Failed(text) = state_after_exit(&ExitInfo { code: Some(77), ..base.clone() }) else { panic!() };
        assert!(text.contains("error code 77"));
        let CardState::Failed(text) = state_after_exit(&ExitInfo { connected: true, ..base.clone() }) else { panic!() };
        assert!(text.contains("unexpectedly"));
        // A dropped connection after the session was up is still a failure.
        let CardState::Failed(_) = state_after_exit(&ExitInfo { connected: true, code: Some(141), ..base }) else { panic!() };
    }

    #[test]
    fn card_activity() {
        assert!(CardState::Connecting.is_active() && CardState::Connected.is_active());
        assert!(!CardState::Idle.is_active() && !CardState::Failed("x".into()).is_active());
    }

    #[test]
    fn credential_failures() {
        assert!(is_credential_failure(Some(154), None));
        assert!(is_credential_failure(None, freerdp::explain_log_line("x ERRCONNECT_LOGON_FAILURE y")));
        assert!(!is_credential_failure(Some(141), None));
        assert!(!is_credential_failure(None, freerdp::explain_log_line("ERRCONNECT_DNS_NAME_NOT_FOUND")));
    }

    #[test]
    fn sign_in_errors_name_the_aadsts_code() {
        let first_line = OAuthError::new("invalid_grant", "AADSTS50126: Invalid username or password.\r\nTrace ID: 1");
        let text = sign_in_error_text(&BackendError::Auth(AuthError::OAuth(first_line)));
        assert_eq!(text, "Sign-in failed: AADSTS50126: Invalid username or password.");
        let later = OAuthError::new("access_denied", "The user declined.\r\nAADSTS65004: consent declined");
        let text = sign_in_error_text(&BackendError::Auth(AuthError::OAuth(later)));
        assert_eq!(text, "Sign-in failed: The user declined (error AADSTS65004).");
        let text = sign_in_error_text(&BackendError::Auth(AuthError::Timeout));
        assert_eq!(text, "Sign-in failed: timed out waiting for sign-in.");
    }

    #[test]
    fn banner_only_for_unusable_clients() {
        let good = Client { kind: ClientKind::Sdl, path: "/usr/bin/sdl-freerdp3".into(), version: Some((3, 33, 1)) };
        let clients = Clients { sdl: Some(good), x11: None };
        assert_eq!(client_banner(&clients, ClientKind::Sdl), None);
        let missing = client_banner(&clients, ClientKind::X11).unwrap();
        assert!(missing.contains("xfreerdp3") && missing.contains("not installed") && missing.contains("package manager"));
        let old = Client { kind: ClientKind::Sdl, path: "/usr/bin/sdl-freerdp3".into(), version: Some((3, 10, 0)) };
        let text = client_banner(&Clients { sdl: Some(old), x11: None }, ClientKind::Sdl).unwrap();
        assert!(text.contains("3.10.0") && text.contains("3.32.0 or later"), "{text}");
    }

    #[test]
    fn scales_round_to_percent() {
        assert_eq!(scale_percent(1.45), Some(145));
        assert_eq!(scale_percent(1.0), Some(100));
        assert_eq!(scale_percent(1.4499), Some(145));
        assert_eq!(scale_percent(0.0), None);
        assert_eq!(scale_percent(f64::NAN), None);
        assert_eq!(password_key("o.t", "abc"), "o.t/abc");
    }

    fn resource(title: &str, workspace: &str) -> Resource {
        Resource {
            id: title.into(),
            title: title.into(),
            kind: ResourceKind::Desktop,
            workspace: workspace.into(),
            tenant_id: String::new(),
            feed_url: "https://rdweb.wvd.microsoft.com/f".into(),
            rdp_url: None,
            icon_url: None,
            folders: vec![],
        }
    }

    #[test]
    fn feed_view_states() {
        assert_eq!(feed_view(None, true, ""), FeedView::Loading);
        assert_eq!(feed_view(None, false, ""), FeedView::Failed);
        let empty = Feed::default();
        assert_eq!(feed_view(Some(&empty), false, ""), FeedView::Empty);
        let feed = Feed::from_resources(vec![resource("Finance", "Contoso")], vec![]);
        assert_eq!(feed_view(Some(&feed), true, ""), FeedView::Grid);
        assert_eq!(feed_view(Some(&feed), false, "lab"), FeedView::NoMatches);
        assert_eq!(feed_view(Some(&feed), false, "contoso"), FeedView::Grid);
    }

    #[test]
    fn warnings_read_as_sentences() {
        assert_eq!(warnings_text(&[]), None);
        assert_eq!(
            warnings_text(&["lab: the service answered HTTP 500".into()]).unwrap(),
            "A workspace could not be loaded: Lab: the service answered HTTP 500."
        );
        let two = warnings_text(&["a.".into(), "b".into()]).unwrap();
        assert_eq!(two, "2 workspaces could not be loaded. A. B.");
    }
}
