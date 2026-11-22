//! Secret storage for refresh tokens and remembered passwords.
//!
//! Uses the desktop keyring through the Secret Service API (KWallet/ksecretd
//! on Plasma, GNOME Keyring elsewhere). When no keyring service is running,
//! secrets fall back to a file only the current user can read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::consts::{APP_ID, APP_NAME};
use crate::paths;

pub const KIND_REFRESH_TOKEN: &str = "refresh-token";
pub const KIND_PASSWORD: &str = "password";

pub struct SecretStore {
    backend: Backend,
}

enum Backend {
    Keyring(oo7::Keyring),
    File { path: PathBuf, lock: Mutex<()> },
    Memory(Mutex<HashMap<String, String>>),
}

impl SecretStore {
    /// Open the desktop keyring, falling back to a private file.
    pub async fn open() -> Self {
        match oo7::Keyring::new().await {
            Ok(keyring) => {
                if let Err(err) = keyring.unlock().await {
                    tracing::warn!("could not unlock the keyring: {err}");
                }
                Self { backend: Backend::Keyring(keyring) }
            }
            Err(err) => {
                tracing::warn!("no keyring service available ({err}); using a private file instead");
                Self { backend: Backend::File { path: paths::data_dir().join("secrets.json"), lock: Mutex::new(()) } }
            }
        }
    }

    /// Volatile store, for tests and demo mode.
    pub fn in_memory() -> Self {
        Self { backend: Backend::Memory(Mutex::new(HashMap::new())) }
    }

    /// Where secrets are kept, for display in the UI.
    pub fn description(&self) -> &'static str {
        match self.backend {
            Backend::Keyring(_) => "the desktop keyring",
            Backend::File { .. } => "a private file (no keyring service is running)",
            Backend::Memory(_) => "memory only",
        }
    }

    fn attributes<'a>(account: &'a str, kind: &'a str) -> [(&'static str, &'a str); 3] {
        [("application", APP_ID), ("account", account), ("kind", kind)]
    }

    pub async fn get(&self, account: &str, kind: &str) -> Option<String> {
        match &self.backend {
            Backend::Keyring(keyring) => {
                let items = keyring.search_items(&Self::attributes(account, kind)).await.ok()?;
                let item = items.first()?;
                let secret = item.secret().await.ok()?;
                String::from_utf8(secret.as_bytes().to_vec()).ok()
            }
            Backend::File { path, lock } => {
                let _guard = lock.lock().unwrap();
                read_file_store(path).remove(&file_key(account, kind))
            }
            Backend::Memory(map) => map.lock().unwrap().get(&file_key(account, kind)).cloned(),
        }
    }

    pub async fn set(&self, account: &str, kind: &str, secret: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Keyring(keyring) => keyring
                .create_item(
                    &format!("{APP_NAME} {kind} ({account})"),
                    &Self::attributes(account, kind),
                    secret,
                    true,
                )
                .await
                .map_err(|err| err.to_string()),
            Backend::File { path, lock } => {
                let _guard = lock.lock().unwrap();
                let mut all = read_file_store(path);
                all.insert(file_key(account, kind), secret.to_owned());
                write_file_store(path, &all)
            }
            Backend::Memory(map) => {
                map.lock().unwrap().insert(file_key(account, kind), secret.to_owned());
                Ok(())
            }
        }
    }

    pub async fn delete(&self, account: &str, kind: &str) {
        match &self.backend {
            Backend::Keyring(keyring) => {
                if let Err(err) = keyring.delete(&Self::attributes(account, kind)).await {
                    tracing::warn!("could not remove a keyring entry: {err}");
                }
            }
            Backend::File { path, lock } => {
                let _guard = lock.lock().unwrap();
                let mut all = read_file_store(path);
                if all.remove(&file_key(account, kind)).is_some() {
                    let _ = write_file_store(path, &all);
                }
            }
            Backend::Memory(map) => {
                map.lock().unwrap().remove(&file_key(account, kind));
            }
        }
    }

    pub async fn delete_passwords_for_account(&self, account_id: &str) {
        let prefix = format!("{account_id}/");
        let suffix = format!("/{KIND_PASSWORD}");
        match &self.backend {
            Backend::Keyring(keyring) => {
                match keyring.search_items(&[("application", APP_ID), ("kind", KIND_PASSWORD)]).await {
                    Ok(items) => {
                        for item in items {
                            if let Ok(attrs) = item.attributes().await
                                && attrs.get("account").is_some_and(|a| a.starts_with(&prefix))
                                && let Err(err) = item.delete().await
                            {
                                tracing::warn!("could not remove a keyring password entry: {err}");
                            }
                        }
                    }
                    Err(err) => tracing::warn!("could not search keyring password entries: {err}"),
                }
            }
            Backend::File { path, lock } => {
                let _guard = lock.lock().unwrap();
                let mut all = read_file_store(path);
                let before = all.len();
                all.retain(|k, _| !(k.starts_with(&prefix) && k.ends_with(&suffix)));
                if all.len() != before {
                    let _ = write_file_store(path, &all);
                }
            }
            Backend::Memory(map) => {
                map.lock().unwrap().retain(|k, _| !(k.starts_with(&prefix) && k.ends_with(&suffix)));
            }
        }
    }
}

fn file_key(account: &str, kind: &str) -> String {
    format!("{account}/{kind}")
}

fn read_file_store(path: &Path) -> HashMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_file_store(path: &Path, all: &HashMap<String, String>) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(all).map_err(|err| err.to_string())?;
    paths::write_private_file(path, &json).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_roundtrip() {
        let store = SecretStore::in_memory();
        assert_eq!(store.get("acct", KIND_REFRESH_TOKEN).await, None);
        store.set("acct", KIND_REFRESH_TOKEN, "rt-1").await.unwrap();
        store.set("acct", KIND_PASSWORD, "pw").await.unwrap();
        assert_eq!(store.get("acct", KIND_REFRESH_TOKEN).await.as_deref(), Some("rt-1"));
        store.set("acct", KIND_REFRESH_TOKEN, "rt-2").await.unwrap();
        assert_eq!(store.get("acct", KIND_REFRESH_TOKEN).await.as_deref(), Some("rt-2"));
        store.delete("acct", KIND_REFRESH_TOKEN).await;
        assert_eq!(store.get("acct", KIND_REFRESH_TOKEN).await, None);
        assert_eq!(store.get("acct", KIND_PASSWORD).await.as_deref(), Some("pw"));
    }

    #[tokio::test]
    async fn file_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cloudrdpapp-store-{}", crate::util::random_token(6)));
        let store = SecretStore { backend: Backend::File { path: dir.join("secrets.json"), lock: Mutex::new(()) } };
        store.set("a", KIND_REFRESH_TOKEN, "one").await.unwrap();
        store.set("b", KIND_REFRESH_TOKEN, "two").await.unwrap();
        assert_eq!(store.get("a", KIND_REFRESH_TOKEN).await.as_deref(), Some("one"));
        store.delete("a", KIND_REFRESH_TOKEN).await;
        assert_eq!(store.get("a", KIND_REFRESH_TOKEN).await, None);
        assert_eq!(store.get("b", KIND_REFRESH_TOKEN).await.as_deref(), Some("two"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn deletes_only_passwords_for_the_given_account() {
        let memory = SecretStore::in_memory();
        let dir = std::env::temp_dir().join(format!("cloudrdpapp-store-{}", crate::util::random_token(6)));
        let file = SecretStore { backend: Backend::File { path: dir.join("secrets.json"), lock: Mutex::new(()) } };

        for store in [&memory, &file] {
            store.set("acct1", KIND_REFRESH_TOKEN, "rt1").await.unwrap();
            store.set("acct1/desk1", KIND_PASSWORD, "pw1").await.unwrap();
            store.set("acct1/desk2", KIND_PASSWORD, "pw2").await.unwrap();
            store.set("acct2/desk1", KIND_PASSWORD, "pw3").await.unwrap();

            store.delete_passwords_for_account("acct1").await;

            assert_eq!(store.get("acct1", KIND_REFRESH_TOKEN).await.as_deref(), Some("rt1"));
            assert_eq!(store.get("acct1/desk1", KIND_PASSWORD).await, None);
            assert_eq!(store.get("acct1/desk2", KIND_PASSWORD).await, None);
            assert_eq!(store.get("acct2/desk1", KIND_PASSWORD).await.as_deref(), Some("pw3"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
