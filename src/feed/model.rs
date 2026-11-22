//! The data model of a workspace feed.

use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    /// A full remote desktop (AVD session desktop or Windows 365 Cloud PC).
    Desktop,
    /// A single published application.
    RemoteApp,
}

impl ResourceKind {
    pub fn label(self) -> &'static str {
        match self {
            ResourceKind::Desktop => "Desktop",
            ResourceKind::RemoteApp => "App",
        }
    }
}

/// One desktop or app from a workspace feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    /// Identifier within the feed.
    pub id: String,
    pub title: String,
    pub kind: ResourceKind,
    /// Workspace the resource is published in.
    pub workspace: String,
    #[serde(default)]
    pub tenant_id: String,
    /// URL of the workspace feed this resource came from.
    pub feed_url: String,
    /// Where to download the .rdp connection file.
    #[serde(default)]
    pub rdp_url: Option<String>,
    /// Best available icon (PNG preferred).
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Feed folders the resource is filed under, e.g. `/Finance`.
    #[serde(default)]
    pub folders: Vec<String>,
}

impl Resource {
    /// Stable key for settings overrides, sessions and cache files.
    pub fn key(&self) -> String {
        util::short_hash(&format!("{}\n{}", self.feed_url, self.id))
    }
}

/// A named group of resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub resources: Vec<Resource>,
}

/// Everything an account can connect to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub workspaces: Vec<Workspace>,
    /// Tenant feeds that failed to load, with the reason.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Feed {
    pub fn is_empty(&self) -> bool {
        self.workspaces.iter().all(|w| w.resources.is_empty())
    }

    pub fn resource_count(&self) -> usize {
        self.workspaces.iter().map(|w| w.resources.len()).sum()
    }

    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.workspaces.iter().flat_map(|w| w.resources.iter())
    }

    pub fn find(&self, key: &str) -> Option<&Resource> {
        self.resources().find(|r| r.key() == key)
    }

    /// Group a flat list by workspace, keeping feed order. Desktops sort
    /// before apps; names sort case-insensitively within each kind.
    pub fn from_resources(mut resources: Vec<Resource>, warnings: Vec<String>) -> Self {
        let mut workspaces: Vec<Workspace> = Vec::new();
        resources.sort_by(|a, b| {
            (a.kind == ResourceKind::RemoteApp)
                .cmp(&(b.kind == ResourceKind::RemoteApp))
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        for resource in resources {
            match workspaces.iter_mut().find(|w| w.name == resource.workspace) {
                Some(workspace) => workspace.resources.push(resource),
                None => workspaces.push(Workspace { name: resource.workspace.clone(), resources: vec![resource] }),
            }
        }
        workspaces.sort_by_key(|w| w.name.to_lowercase());
        Self { workspaces, warnings }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(id: &str, title: &str, kind: ResourceKind, workspace: &str) -> Resource {
        Resource {
            id: id.into(),
            title: title.into(),
            kind,
            workspace: workspace.into(),
            tenant_id: "t".into(),
            feed_url: format!("https://rdweb.wvd.microsoft.com/feed/{workspace}"),
            rdp_url: None,
            icon_url: None,
            folders: vec![],
        }
    }

    #[test]
    fn groups_and_sorts() {
        let feed = Feed::from_resources(
            vec![
                resource("1", "Zeta App", ResourceKind::RemoteApp, "Prod"),
                resource("2", "beta Desktop", ResourceKind::Desktop, "Prod"),
                resource("3", "Alpha Desktop", ResourceKind::Desktop, "Prod"),
                resource("4", "Cloud PC", ResourceKind::Desktop, "Windows 365"),
            ],
            vec![],
        );
        assert_eq!(feed.workspaces.len(), 2);
        assert_eq!(feed.resource_count(), 4);
        let prod: Vec<_> = feed.workspaces[0].resources.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(prod, ["Alpha Desktop", "beta Desktop", "Zeta App"]);
        assert_eq!(feed.workspaces[1].name, "Windows 365");
        assert!(!feed.is_empty());
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        let a = resource("1", "A", ResourceKind::Desktop, "W1");
        let b = resource("1", "A", ResourceKind::Desktop, "W2");
        assert_eq!(a.key(), a.clone().key());
        assert_ne!(a.key(), b.key());
        assert_eq!(a.key().len(), 16);
        let feed = Feed::from_resources(vec![a.clone(), b], vec![]);
        assert_eq!(feed.find(&a.key()), Some(&a));
        assert_eq!(feed.find("missing"), None);
    }
}
