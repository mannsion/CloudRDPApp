//! What the GUI talks to: the real [`Backend`], or the offline demo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::auth::store::KIND_PASSWORD;
use crate::auth::{Account, PendingLogin};
use crate::backend::{Backend, BackendError, Clients, Session};
use crate::config::ConnectionSettings;
use crate::feed::{Feed, Resource};
use crate::rdp::session::EventSink;

use super::demo::{Demo, DemoSession};
use super::logic::password_key;

#[derive(Clone)]
pub enum Service {
    Real(Backend),
    Demo(Arc<Demo>),
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Service::Real(_) => "Service::Real",
            Service::Demo(_) => "Service::Demo",
        })
    }
}

/// A started paste-the-address sign-in.
pub enum ManualLogin {
    Real(PendingLogin),
    Demo,
}

/// A running connection.
#[derive(Debug)]
pub enum SessionHandle {
    Real(Session),
    Demo(DemoSession),
}

impl SessionHandle {
    pub fn stop(&mut self) {
        match self {
            SessionHandle::Real(session) => session.stop(),
            SessionHandle::Demo(session) => session.stop(),
        }
    }

    pub fn log_file(&self) -> PathBuf {
        match self {
            SessionHandle::Real(session) => session.log_file().to_path_buf(),
            SessionHandle::Demo(session) => session.log_file.clone(),
        }
    }
}

impl Service {
    pub fn clients(&self) -> Clients {
        match self {
            Service::Real(backend) => backend.clients(),
            Service::Demo(demo) => demo.clients(),
        }
    }

    pub async fn redetect_clients(&self, sdl: &str, x11: &str) -> Clients {
        match self {
            Service::Real(backend) => backend.redetect_clients(sdl, x11).await,
            Service::Demo(demo) => demo.clients(),
        }
    }

    pub async fn restore_account(&self, account: &Account) -> bool {
        match self {
            Service::Real(backend) => backend.restore_account(account).await,
            Service::Demo(_) => false,
        }
    }

    pub async fn sign_in(&self) -> Result<Account, BackendError> {
        match self {
            Service::Real(backend) => backend.sign_in().await,
            Service::Demo(demo) => demo.sign_in().await,
        }
    }

    pub fn start_manual_sign_in(&self) -> ManualLogin {
        match self {
            Service::Real(backend) => ManualLogin::Real(backend.start_manual_sign_in()),
            Service::Demo(_) => ManualLogin::Demo,
        }
    }

    pub async fn finish_manual_sign_in(&self, pending: &ManualLogin, pasted: &str) -> Result<Account, BackendError> {
        match (self, pending) {
            (Service::Real(backend), ManualLogin::Real(pending)) => backend.finish_manual_sign_in(pending, pasted).await,
            (Service::Demo(demo), _) => demo.finish_manual_sign_in(pasted).await,
            (Service::Real(_), ManualLogin::Demo) => unreachable!("a demo sign-in on the real backend"),
        }
    }

    pub async fn sign_out(&self) {
        match self {
            Service::Real(backend) => backend.sign_out().await,
            Service::Demo(demo) => demo.sign_out(),
        }
    }

    pub fn cached_feed(&self, account: &Account) -> Option<Feed> {
        match self {
            Service::Real(backend) => backend.cached_feed(account),
            Service::Demo(_) => None,
        }
    }

    pub async fn fetch_feed(&self) -> Result<Feed, BackendError> {
        match self {
            Service::Real(backend) => backend.fetch_feed().await,
            Service::Demo(demo) => demo.fetch_feed().await,
        }
    }

    pub async fn icon(&self, resource: &Resource) -> Option<PathBuf> {
        match self {
            Service::Real(backend) => backend.icon(resource).await,
            Service::Demo(_) => None,
        }
    }

    pub async fn connect(
        &self,
        resource: &Resource,
        settings: &ConnectionSettings,
        password: Option<&str>,
        scale: Option<u32>,
        events: EventSink,
    ) -> Result<SessionHandle, BackendError> {
        match self {
            Service::Real(backend) => {
                backend.connect(resource, settings, password, scale, events).await.map(SessionHandle::Real)
            }
            Service::Demo(demo) => demo.connect(&resource.id, &resource.title, password, events).map(SessionHandle::Demo),
        }
    }

    pub async fn connect_rdp(
        &self,
        text: &str,
        title: &str,
        settings: &ConnectionSettings,
        password: Option<&str>,
        scale: Option<u32>,
        events: EventSink,
    ) -> Result<SessionHandle, BackendError> {
        match self {
            Service::Real(backend) => {
                backend.connect_rdp(text, title, settings, password, scale, events).await.map(SessionHandle::Real)
            }
            Service::Demo(demo) => demo.connect("file", title, password, events).map(SessionHandle::Demo),
        }
    }

    pub async fn saved_password(&self, account_id: &str, key: &str) -> Option<String> {
        let id = password_key(account_id, key);
        match self {
            Service::Real(backend) => backend.auth().store().get(&id, KIND_PASSWORD).await,
            Service::Demo(demo) => demo.saved_password(&id),
        }
    }

    pub async fn remember_password(&self, account_id: &str, key: &str, password: &str) -> Result<(), String> {
        let id = password_key(account_id, key);
        match self {
            Service::Real(backend) => backend.auth().store().set(&id, KIND_PASSWORD, password).await,
            Service::Demo(demo) => {
                demo.remember_password(&id, password);
                Ok(())
            }
        }
    }

    pub async fn forget_password(&self, account_id: &str, key: &str) {
        let id = password_key(account_id, key);
        match self {
            Service::Real(backend) => backend.auth().store().delete(&id, KIND_PASSWORD).await,
            Service::Demo(demo) => demo.forget_password(&id),
        }
    }

    /// Where remembered passwords go, for the password dialog.
    pub fn secret_storage(&self) -> &'static str {
        match self {
            Service::Real(backend) => backend.auth().storage_description(),
            Service::Demo(_) => "memory only (demo)",
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        match self {
            Service::Real(_) => crate::paths::logs_dir(),
            Service::Demo(demo) => demo.log_dir().to_path_buf(),
        }
    }
}

/// Title for a session started from a `.rdp` file.
pub fn rdp_file_title(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).filter(|s| !s.trim().is_empty()).unwrap_or("Remote desktop").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdp_file_titles() {
        assert_eq!(rdp_file_title(Path::new("/home/u/Work PC.rdp")), "Work PC");
        assert_eq!(rdp_file_title(Path::new("/")), "Remote desktop");
    }
}
