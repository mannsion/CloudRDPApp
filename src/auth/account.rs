//! The signed-in work account, read from the ID token.

use serde::{Deserialize, Serialize};

use super::AuthError;
use crate::util;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Directory (tenant) id.
    pub tenant_id: String,
    /// Object id of the user in that tenant.
    pub object_id: String,
    /// User principal name, e.g. `alex@contoso.com`.
    pub username: String,
    /// Display name, when the token carries one.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    #[serde(default)]
    tid: Option<String>,
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    upn: Option<String>,
    #[serde(default)]
    unique_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl Account {
    /// Stable identifier used for keyring entries and cache directories.
    pub fn id(&self) -> String {
        format!("{}.{}", self.object_id, self.tenant_id)
    }

    /// Name to show in the UI.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&self.username)
    }

    /// Read the account from an ID token. The token came straight from the
    /// token endpoint over TLS, so its signature is not re-verified here.
    pub fn from_id_token(id_token: &str) -> Result<Self, AuthError> {
        let payload = id_token
            .split('.')
            .nth(1)
            .ok_or_else(|| AuthError::Other("the ID token is malformed".to_owned()))?;
        let bytes = util::b64url_decode(payload)
            .ok_or_else(|| AuthError::Other("the ID token is not valid base64".to_owned()))?;
        let claims: IdClaims = serde_json::from_slice(&bytes)
            .map_err(|err| AuthError::Other(format!("the ID token could not be read: {err}")))?;

        let tenant_id = claims.tid.unwrap_or_default();
        let object_id = claims.oid.or(claims.sub).unwrap_or_default();
        if tenant_id.is_empty() || object_id.is_empty() {
            return Err(AuthError::Other("the ID token does not identify a work account".to_owned()));
        }
        let username = claims
            .preferred_username
            .or(claims.upn)
            .or(claims.unique_name)
            .or(claims.email)
            .unwrap_or_default();
        Ok(Self { tenant_id, object_id, username, name: claims.name })
    }
}

#[cfg(test)]
pub(crate) fn fake_id_token(claims: &serde_json::Value) -> String {
    format!(
        "{}.{}.{}",
        util::b64url_encode(br#"{"alg":"none"}"#),
        util::b64url_encode(claims.to_string().as_bytes()),
        "signature"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claims() {
        let token = fake_id_token(&serde_json::json!({
            "tid": "tenant-1", "oid": "object-1",
            "preferred_username": "alex@contoso.com", "name": "Alex Example"
        }));
        let account = Account::from_id_token(&token).unwrap();
        assert_eq!(account.tenant_id, "tenant-1");
        assert_eq!(account.object_id, "object-1");
        assert_eq!(account.username, "alex@contoso.com");
        assert_eq!(account.display_name(), "Alex Example");
        assert_eq!(account.id(), "object-1.tenant-1");
    }

    #[test]
    fn falls_back_between_username_claims() {
        let token = fake_id_token(&serde_json::json!({"tid": "t", "sub": "s", "upn": "u@x.io"}));
        let account = Account::from_id_token(&token).unwrap();
        assert_eq!(account.object_id, "s");
        assert_eq!(account.username, "u@x.io");
        assert_eq!(account.display_name(), "u@x.io");
    }

    #[test]
    fn rejects_garbage() {
        assert!(Account::from_id_token("nope").is_err());
        assert!(Account::from_id_token("a.b.c").is_err());
        let token = fake_id_token(&serde_json::json!({"name": "no ids"}));
        assert!(Account::from_id_token(&token).is_err());
    }
}
