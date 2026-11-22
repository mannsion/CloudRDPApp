//! Reading and editing `.rdp` connection files.
//!
//! A connection file is a list of `name:type:value` lines, for example
//! `full address:s:rdgateway-r1.wvd.microsoft.com` or `use multimon:i:1`.
//! Unknown lines are preserved untouched, so everything the feed supplies
//! (gateway, host pool, load balancing info) reaches FreeRDP as issued.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Property { name: String, kind: String, value: String },
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RdpFile {
    lines: Vec<Line>,
}

impl RdpFile {
    pub fn parse(text: &str) -> Self {
        let mut lines = Vec::new();
        for raw in text.trim_start_matches('\u{feff}').lines() {
            let raw = raw.trim_end_matches('\r');
            if raw.trim().is_empty() {
                continue;
            }
            lines.push(parse_line(raw));
        }
        Self { lines }
    }

    /// Value of the first property called `name` (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.lines.iter().find_map(|line| match line {
            Line::Property { name: n, value, .. } if n.eq_ignore_ascii_case(name) => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn get_int(&self, name: &str) -> Option<i64> {
        self.get(name)?.trim().parse().ok()
    }

    /// Set a property, replacing every existing line with that name.
    pub fn set(&mut self, name: &str, kind: char, value: &str) {
        let replacement = Line::Property { name: name.to_owned(), kind: kind.to_string(), value: value.to_owned() };
        let mut replaced = false;
        self.lines.retain_mut(|line| match line {
            Line::Property { name: n, .. } if n.eq_ignore_ascii_case(name) => {
                if replaced {
                    false
                } else {
                    *line = replacement.clone();
                    replaced = true;
                    true
                }
            }
            _ => true,
        });
        if !replaced {
            self.lines.push(replacement);
        }
    }

    pub fn set_int(&mut self, name: &str, value: i64) {
        self.set(name, 'i', &value.to_string());
    }

    pub fn set_str(&mut self, name: &str, value: &str) {
        self.set(name, 's', value);
    }

    /// Remove every line called `name`. Returns true if any was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| !matches!(line, Line::Property { name: n, .. } if n.eq_ignore_ascii_case(name)));
        self.lines.len() != before
    }

    pub fn property_count(&self) -> usize {
        self.lines.iter().filter(|line| matches!(line, Line::Property { .. })).count()
    }

    /// True when the file publishes a single application (RemoteApp).
    pub fn is_remote_app(&self) -> bool {
        self.get_int("remoteapplicationmode") == Some(1)
    }

    /// True when the host pool offers Microsoft Entra single sign-on.
    pub fn offers_entra_sso(&self) -> bool {
        self.get_int("enablerdsaadauth") == Some(1)
    }

    /// True when the file targets Azure Virtual Desktop / Windows 365.
    pub fn is_avd(&self) -> bool {
        self.get("resourceprovider").map(|v| v.eq_ignore_ascii_case("arm")).unwrap_or(false)
            || self.get("wvd endpoint pool").is_some()
    }

    /// Serialise with the CRLF line endings mstsc writes.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Property { name, kind, value } => {
                    out.push_str(name);
                    out.push(':');
                    out.push_str(kind);
                    out.push(':');
                    out.push_str(value);
                }
                Line::Other(raw) => out.push_str(raw),
            }
            out.push_str("\r\n");
        }
        out
    }
}

fn parse_line(raw: &str) -> Line {
    let mut parts = raw.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), Some(kind), Some(value))
            if !name.trim().is_empty() && kind.len() == 1 && kind.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            Line::Property { name: name.trim().to_owned(), kind: kind.to_owned(), value: value.to_owned() }
        }
        _ => Line::Other(raw.to_owned()),
    }
}

#[cfg(test)]
pub(crate) const SAMPLE_AVD_RDP: &str = "gatewayusagemethod:i:1\r\n\
gatewayprofileusagemethod:i:1\r\n\
authentication level:i:1\r\n\
gatewaybrokeringtype:i:1\r\n\
wvd endpoint pool:s:11111111-2222-3333-4444-555555555555\r\n\
geo:s:US__False\r\n\
armpath:s:/subscriptions/aaaa/resourcegroups/rg/providers/Microsoft.DesktopVirtualization/hostpools/Pool\r\n\
aadtenantid:s:99999999-8888-7777-6666-555555555555\r\n\
full address:s:rdgateway-r1.wvd.microsoft.com\r\n\
alternate full address:s:rdgateway-r1.wvd.microsoft.com\r\n\
diagnosticserviceurl:s:https://rdweb-g-us-r1.wvd.microsoft.com/api/arm/DiagnosticEvents/v1\r\n\
hubdiscoverygeourl:s:https://rdweb-g-us-r1.wvd.microsoft.com/api/arm/hubdiscovery?resourceId=abc\r\n\
resourceprovider:s:arm\r\n\
gatewayhostname:s:afdfp-rdgateway-r1.wvd.microsoft.com:443\r\n\
loadbalanceinfo:s:mth://localhost/11111111-2222/33333333-4444\r\n\
workspace id:s:77777777-0000-0000-0000-000000000000\r\n\
promptcredentialonce:i:1\r\n\
gatewaycredentialssource:i:0\r\n\
use multimon:i:1\r\n\
smart sizing:i:1\r\n\
enablerdsaadauth:i:1\r\n\
targetisaadjoined:i:1\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reads_properties() {
        let rdp = RdpFile::parse(SAMPLE_AVD_RDP);
        assert_eq!(rdp.property_count(), 22);
        assert_eq!(rdp.get("full address"), Some("rdgateway-r1.wvd.microsoft.com"));
        assert_eq!(rdp.get("FULL ADDRESS"), Some("rdgateway-r1.wvd.microsoft.com"));
        assert_eq!(rdp.get_int("use multimon"), Some(1));
        // Values keep their own colons.
        assert_eq!(rdp.get("gatewayhostname"), Some("afdfp-rdgateway-r1.wvd.microsoft.com:443"));
        assert_eq!(rdp.get("loadbalanceinfo"), Some("mth://localhost/11111111-2222/33333333-4444"));
        assert!(rdp.is_avd());
        assert!(rdp.offers_entra_sso());
        assert!(!rdp.is_remote_app());
        assert_eq!(rdp.get("missing"), None);
    }

    #[test]
    fn roundtrips_unchanged() {
        let rdp = RdpFile::parse(SAMPLE_AVD_RDP);
        assert_eq!(rdp.serialize(), SAMPLE_AVD_RDP);
    }

    #[test]
    fn edits_properties() {
        let mut rdp = RdpFile::parse("use multimon:i:1\nUse Multimon:i:1\nfull address:s:host\n");
        rdp.set_int("use multimon", 0);
        assert_eq!(rdp.serialize(), "use multimon:i:0\r\nfull address:s:host\r\n");
        rdp.set_str("drivestoredirect", "*");
        assert_eq!(rdp.get("drivestoredirect"), Some("*"));
        assert!(rdp.remove("full address"));
        assert!(!rdp.remove("full address"));
        assert_eq!(rdp.serialize(), "use multimon:i:0\r\ndrivestoredirect:s:*\r\n");
    }

    #[test]
    fn keeps_unrecognised_lines() {
        let text = "\u{feff}full address:s:host\r\nnot a property line\r\n\r\nsignature:s:AQABAAEAAA==\r\n";
        let rdp = RdpFile::parse(text);
        assert_eq!(rdp.property_count(), 2);
        assert_eq!(rdp.serialize(), "full address:s:host\r\nnot a property line\r\nsignature:s:AQABAAEAAA==\r\n");
    }

    #[test]
    fn detects_remote_apps() {
        let rdp = RdpFile::parse("remoteapplicationmode:i:1\nremoteapplicationprogram:s:||excel\n");
        assert!(rdp.is_remote_app());
        assert!(!rdp.offers_entra_sso());
        assert!(!rdp.is_avd());
    }
}
