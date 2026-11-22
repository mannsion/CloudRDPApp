//! Parsing of the feed discovery and workspace feed XML documents.
//!
//! Elements are matched by local name so the parser does not depend on the
//! exact namespace URIs (`…/ts/2014/03/tswfdiscovery`, `…/ts/2007/05/tswf`).

use roxmltree::{Document, Node};

use super::FeedError;
use super::model::{Resource, ResourceKind};

/// One entry of the feed discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFeed {
    pub feed_url: String,
    pub tenant_id: String,
    pub workspace_name: String,
}

fn parse_document(xml: &str) -> Result<Document<'_>, FeedError> {
    Document::parse(xml.trim_start_matches('\u{feff}').trim())
        .map_err(|err| FeedError::Malformed(err.to_string()))
}

fn elements<'a, 'input>(root: Node<'a, 'input>, name: &'static str) -> impl Iterator<Item = Node<'a, 'input>> {
    root.descendants().filter(move |node| node.is_element() && node.tag_name().name() == name)
}

/// Attribute lookup that ignores ASCII case (`FeedURL` vs `FeedUrl`).
fn attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
        .filter(|v| !v.trim().is_empty())
}

/// Parse the answer of `/api/arm/feeddiscovery`.
pub fn parse_discovery(xml: &str, base_url: &str) -> Result<Vec<DiscoveredFeed>, FeedError> {
    let doc = parse_document(xml)?;
    let mut feeds = Vec::new();
    for node in elements(doc.root(), "TenantFeedURL") {
        let Some(feed_url) = attr(node, "FeedURL").and_then(|u| resolve(base_url, u)) else {
            continue;
        };
        let workspace_name = attr(node, "WorkspaceName")
            .or_else(|| attr(node, "TenantDisplayName"))
            .unwrap_or("")
            .to_owned();
        feeds.push(DiscoveredFeed {
            feed_url,
            tenant_id: attr(node, "TenantId").unwrap_or("").to_owned(),
            workspace_name,
        });
    }
    Ok(feeds)
}

/// Parse one workspace feed (`ResourceCollection`).
pub fn parse_feed(xml: &str, source: &DiscoveredFeed) -> Result<Vec<Resource>, FeedError> {
    let doc = parse_document(xml)?;
    let publisher_name = elements(doc.root(), "Publisher").next().and_then(|p| attr(p, "Name")).unwrap_or("");
    let workspace = [source.workspace_name.as_str(), publisher_name, "Workspace"]
        .into_iter()
        .find(|name| !name.trim().is_empty())
        .unwrap_or("Workspace")
        .to_owned();

    let mut resources = Vec::new();
    for node in elements(doc.root(), "Resource") {
        let Some(id) = attr(node, "ID") else { continue };
        let kind = match attr(node, "Type") {
            Some(t) if t.eq_ignore_ascii_case("RemoteApp") => ResourceKind::RemoteApp,
            _ => ResourceKind::Desktop,
        };
        let rdp_url = elements(node, "ResourceFile")
            .filter_map(|file| attr(file, "URL"))
            .find_map(|url| resolve(&source.feed_url, url));
        let folders = elements(node, "Folder")
            .filter_map(|folder| attr(folder, "Name"))
            .filter(|name| *name != "/")
            .map(str::to_owned)
            .collect();
        resources.push(Resource {
            id: id.to_owned(),
            title: attr(node, "Title").or_else(|| attr(node, "Alias")).unwrap_or(id).to_owned(),
            kind,
            workspace: workspace.clone(),
            tenant_id: source.tenant_id.clone(),
            feed_url: source.feed_url.clone(),
            rdp_url,
            icon_url: best_icon(node).and_then(|url| resolve(&source.feed_url, url)),
            folders,
        });
    }
    Ok(resources)
}

/// Pick the icon to show: the largest PNG up to 256 px, otherwise whatever
/// the feed offers (`IconRaw` is an .ico file).
fn best_icon<'a>(resource: Node<'a, '_>) -> Option<&'a str> {
    let mut best: Option<(u32, &str)> = None;
    let mut fallback = None;
    for node in resource.descendants().filter(|n| n.is_element()) {
        let name = node.tag_name().name();
        if !name.starts_with("Icon") || name == "Icons" {
            continue;
        }
        let Some(url) = attr(node, "FileURL") else { continue };
        let is_png = attr(node, "FileType").map(|t| t.eq_ignore_ascii_case("png")).unwrap_or(false)
            || url.to_ascii_lowercase().contains(".png");
        let size = name[4..]
            .parse::<u32>()
            .ok()
            .or_else(|| attr(node, "Dimensions").and_then(|d| d.split('x').next()?.trim().parse().ok()));
        match (is_png, size) {
            (true, Some(size)) if size <= 256 => {
                if best.map(|(current, _)| size > current).unwrap_or(true) {
                    best = Some((size, url));
                }
            }
            _ => {
                if fallback.is_none() {
                    fallback = Some(url);
                }
            }
        }
    }
    best.map(|(_, url)| url).or(fallback)
}

/// Resolve a possibly relative URL against the document it came from.
fn resolve(base: &str, target: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    base.join(target.trim()).ok().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCOVERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<TenantFeedURLs xmlns="http://schemas.microsoft.com/ts/2014/03/tswfdiscovery" ARMResources="true">
  <TenantFeedURL FeedURL="https://rdweb.wvd.microsoft.com/api/arm/feed/11111111-aaaa/22222222-bbbb"
                 TenantId="11111111-aaaa" TenantDisplayName="Contoso" WorkspaceName="Engineering" Geo="US"/>
  <TenantFeedURL FeedURL="/api/arm/feed/11111111-aaaa/33333333-cccc"
                 TenantId="11111111-aaaa" TenantDisplayName="Windows 365"/>
  <TenantFeedURL TenantId="broken"/>
</TenantFeedURLs>"#;

    const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<ResourceCollection PubDate="2026-11-20T10:00:00Z" SchemaVersion="2.1"
    xmlns="http://schemas.microsoft.com/ts/2007/05/tswf">
  <Publisher LastUpdated="2026-11-20T10:00:00Z" Name="Contoso Engineering" ID="pub" Description="" SupportsReconnect="true">
    <SubFolders><Folder Name="/"/><Folder Name="/Tools"/></SubFolders>
    <Resources>
      <Resource ID="desk-1" Alias="SessionDesktop" Title="Dev Desktop" LastUpdated="2026-11-01T00:00:00Z" Type="Desktop" ShowByDefault="True">
        <Icons>
          <IconRaw FileType="Ico" FileURL="/api/arm/icon/desk-1/raw"/>
          <Icon16 Dimensions="16x16" FileType="Png" FileURL="/api/arm/icon/desk-1/16"/>
          <Icon32 Dimensions="32x32" FileType="Png" FileURL="/api/arm/icon/desk-1/32"/>
          <Icon256 Dimensions="256x256" FileType="Png" FileURL="/api/arm/icon/desk-1/256"/>
        </Icons>
        <FileExtensions/>
        <Folders><Folder Name="/"/></Folders>
        <HostingTerminalServers>
          <HostingTerminalServer>
            <ResourceFile FileExtension=".rdp" URL="/api/arm/rdp/desk-1?x=1&amp;y=2"/>
            <TerminalServerRef Ref="ts"/>
          </HostingTerminalServer>
        </HostingTerminalServers>
      </Resource>
      <Resource ID="app-1" Alias="excel" Title="Excel" Type="RemoteApp" ExecutableName="excel.exe">
        <Icons><IconRaw FileType="Ico" FileURL="https://rdweb.wvd.microsoft.com/api/arm/icon/app-1/raw"/></Icons>
        <Folders><Folder Name="/Tools"/></Folders>
        <HostingTerminalServers>
          <HostingTerminalServer>
            <ResourceFile FileExtension=".rdp" URL="https://rdweb.wvd.microsoft.com/api/arm/rdp/app-1"/>
          </HostingTerminalServer>
        </HostingTerminalServers>
      </Resource>
      <Resource Title="No id, skipped"/>
    </Resources>
    <TerminalServers><TerminalServer ID="ts" Name="ts" LastUpdated="2026-11-01T00:00:00Z"/></TerminalServers>
  </Publisher>
</ResourceCollection>"#;

    fn discovered() -> Vec<DiscoveredFeed> {
        parse_discovery(DISCOVERY, "https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery").unwrap()
    }

    #[test]
    fn discovery_lists_tenant_feeds() {
        let feeds = discovered();
        assert_eq!(feeds.len(), 2, "the entry without a FeedURL is skipped");
        assert_eq!(feeds[0].workspace_name, "Engineering");
        assert_eq!(feeds[0].tenant_id, "11111111-aaaa");
        assert_eq!(feeds[0].feed_url, "https://rdweb.wvd.microsoft.com/api/arm/feed/11111111-aaaa/22222222-bbbb");
        // Relative URL resolved, WorkspaceName falls back to TenantDisplayName.
        assert_eq!(feeds[1].feed_url, "https://rdweb.wvd.microsoft.com/api/arm/feed/11111111-aaaa/33333333-cccc");
        assert_eq!(feeds[1].workspace_name, "Windows 365");
    }

    #[test]
    fn feed_lists_resources() {
        let source = &discovered()[0];
        let resources = parse_feed(FEED, source).unwrap();
        assert_eq!(resources.len(), 2);

        let desktop = &resources[0];
        assert_eq!(desktop.id, "desk-1");
        assert_eq!(desktop.title, "Dev Desktop");
        assert_eq!(desktop.kind, ResourceKind::Desktop);
        assert_eq!(desktop.workspace, "Engineering");
        assert_eq!(desktop.rdp_url.as_deref(), Some("https://rdweb.wvd.microsoft.com/api/arm/rdp/desk-1?x=1&y=2"));
        assert_eq!(desktop.icon_url.as_deref(), Some("https://rdweb.wvd.microsoft.com/api/arm/icon/desk-1/256"));
        assert!(desktop.folders.is_empty(), "the root folder is not listed");

        let app = &resources[1];
        assert_eq!(app.kind, ResourceKind::RemoteApp);
        assert_eq!(app.folders, ["/Tools"]);
        // Only an .ico is offered: use it rather than nothing.
        assert_eq!(app.icon_url.as_deref(), Some("https://rdweb.wvd.microsoft.com/api/arm/icon/app-1/raw"));
    }

    #[test]
    fn workspace_name_falls_back_to_publisher() {
        let source = DiscoveredFeed {
            feed_url: "https://rdweb.wvd.microsoft.com/f".into(),
            tenant_id: String::new(),
            workspace_name: String::new(),
        };
        let resources = parse_feed(FEED, &source).unwrap();
        assert_eq!(resources[0].workspace, "Contoso Engineering");
    }

    #[test]
    fn tolerates_bom_and_missing_namespace() {
        let xml = "\u{feff}<TenantFeedURLs><TenantFeedURL FeedUrl=\"https://rdweb.wvd.microsoft.com/f0\"/></TenantFeedURLs>";
        let feeds = parse_discovery(xml, "https://rdweb.wvd.microsoft.com/").unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].feed_url, "https://rdweb.wvd.microsoft.com/f0");
    }

    #[test]
    fn rejects_non_xml() {
        assert!(matches!(parse_discovery("<html>oops", "https://x/"), Err(FeedError::Malformed(_))));
        assert!(parse_discovery("<Empty/>", "https://x/").unwrap().is_empty());
    }
}
