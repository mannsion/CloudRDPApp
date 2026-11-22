//! Per-user registration of this program as the handler for `ms-appx-web:`
//! addresses, so the browser can hand back the broker sign-in redirect
//! (see `auth::callback`).
//!
//! A hidden desktop entry in `~/.local/share/applications` runs
//! `<this executable> --auth-callback %u`, and `xdg-mime default` makes it the
//! default for `x-scheme-handler/ms-appx-web`. The app checks this before
//! every browser sign-in, so a moved or rebuilt executable (`cargo run`)
//! is picked up; `make install`/`uninstall` call `--register-url-handler`
//! and `--unregister-url-handler`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::consts;
use crate::paths;

/// The MIME type browsers look up for `ms-appx-web:` links.
pub const MIME_TYPE: &str = "x-scheme-handler/ms-appx-web";
/// Desktop file id of the hidden handler entry.
pub const DESKTOP_ID: &str = "io.github.mannsion.CloudRDPApp.AuthCallback.desktop";

/// Command-line flags for `make install` / `make uninstall`.
pub const ARG_REGISTER: &str = "--register-url-handler";
pub const ARG_UNREGISTER: &str = "--unregister-url-handler";

/// The desktop entry that runs `exe --auth-callback %u`.
pub fn desktop_entry(exe: &Path) -> Result<String, String> {
    let exe = exe.to_str().ok_or("the program path is not valid UTF-8")?;
    if !exe.starts_with('/') {
        return Err(format!("the program path {exe} is not absolute"));
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name} sign-in\n\
         Comment=Returns browser sign-ins to {name}\n\
         Exec={exec} {arg} %u\n\
         Icon={icon}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         StartupNotify=false\n\
         MimeType={MIME_TYPE};\n",
        name = consts::APP_NAME,
        exec = exec_quote(exe)?,
        arg = crate::helper::callback::ARG,
        icon = consts::APP_ID,
    ))
}

/// Quote a program path for an `Exec=` key (Desktop Entry Specification,
/// "The Exec key"): reserved characters need double quotes, inside which
/// `"`, `` ` ``, `$` and `\` are backslash-escaped; `%` is doubled; and the
/// value as a whole escapes `\` once more.
pub fn exec_quote(path: &str) -> Result<String, String> {
    if path.chars().any(char::is_control) {
        return Err("the program path contains control characters".to_owned());
    }
    let path = path.replace('%', "%%");
    let plain = path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain {
        return Ok(path);
    }
    let mut quoted = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    Ok(quoted.replace('\\', "\\\\"))
}

/// The commands this module runs, so tests do not touch the real desktop.
pub trait Desktop {
    /// `xdg-mime query default x-scheme-handler/ms-appx-web` (trimmed, may be empty).
    fn query_default(&self) -> Result<String, String>;
    /// `xdg-mime default <DESKTOP_ID> x-scheme-handler/ms-appx-web`.
    fn set_default(&self) -> Result<(), String>;
    /// `update-desktop-database <dir>`, best effort.
    fn refresh(&self, apps_dir: &Path);
}

/// The real commands.
pub struct SystemDesktop;

impl Desktop for SystemDesktop {
    fn query_default(&self) -> Result<String, String> {
        let output = Command::new("xdg-mime")
            .args(["query", "default", MIME_TYPE])
            .output()
            .map_err(|err| format!("xdg-mime could not be run: {err}"))?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn set_default(&self) -> Result<(), String> {
        let status = Command::new("xdg-mime")
            .args(["default", DESKTOP_ID, MIME_TYPE])
            .status()
            .map_err(|err| format!("xdg-mime could not be run: {err}"))?;
        if status.success() { Ok(()) } else { Err(format!("xdg-mime default failed ({status})")) }
    }

    fn refresh(&self, apps_dir: &Path) {
        let _ = Command::new("update-desktop-database").arg("-q").arg(apps_dir).status();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing had to change.
    AlreadyRegistered,
    /// The entry was written or made the default. `replaced` names the
    /// handler that was the default before, if it was another one.
    Registered { replaced: Option<String> },
}

/// Make `exe` the handler, for the current user's real desktop.
pub fn ensure_registered(exe: &Path) -> Result<Outcome, String> {
    ensure_registered_in(&paths::applications_dir(), exe, &SystemDesktop)
}

/// Make `exe` the handler: write the entry in `apps_dir` if it differs and
/// make it the default if it is not. Another handler is replaced only after
/// it has been logged.
pub fn ensure_registered_in(apps_dir: &Path, exe: &Path, desktop: &dyn Desktop) -> Result<Outcome, String> {
    let desired = desktop_entry(exe)?;
    let file = apps_dir.join(DESKTOP_ID);
    let changed = std::fs::read_to_string(&file).ok().as_deref() != Some(desired.as_str());
    if changed {
        std::fs::create_dir_all(apps_dir).map_err(|err| format!("{}: {err}", apps_dir.display()))?;
        write_atomically(&file, desired.as_bytes()).map_err(|err| format!("{}: {err}", file.display()))?;
        desktop.refresh(apps_dir);
    }

    let current = desktop.query_default().unwrap_or_else(|err| {
        tracing::debug!("could not read the ms-appx-web handler: {err}");
        String::new()
    });
    if current == DESKTOP_ID && !changed {
        return Ok(Outcome::AlreadyRegistered);
    }
    // A freshly written entry is always made the explicit default: a query
    // can also answer from mimeinfo.cache, which is not a choice.
    let replaced = (!current.is_empty() && current != DESKTOP_ID).then_some(current);
    if let Some(previous) = &replaced {
        tracing::warn!(
            "replacing {previous} (Exec={}) as the handler for {MIME_TYPE} so that browser sign-ins return to {}",
            find_exec(previous, apps_dir).unwrap_or_else(|| "unknown".to_owned()),
            consts::APP_NAME
        );
    }
    desktop.set_default()?;
    Ok(Outcome::Registered { replaced })
}

/// Remove the entry and every association with it, for the current user.
pub fn unregister() -> Result<(), String> {
    unregister_in(&paths::applications_dir(), &[paths::mimeapps_list(), paths::applications_dir().join("mimeapps.list")], &SystemDesktop)
}

pub fn unregister_in(apps_dir: &Path, mimeapps: &[PathBuf], desktop: &dyn Desktop) -> Result<(), String> {
    let file = apps_dir.join(DESKTOP_ID);
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", file.display())),
    }
    for list in mimeapps {
        let Ok(text) = std::fs::read_to_string(list) else { continue };
        if let Some(updated) = remove_association(&text) {
            write_atomically(list, updated.as_bytes()).map_err(|err| format!("{}: {err}", list.display()))?;
        }
    }
    desktop.refresh(apps_dir);
    Ok(())
}

/// `mimeapps.list` text without [`DESKTOP_ID`] in any `ms-appx-web` entry;
/// `None` when it was not mentioned.
pub fn remove_association(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some((key, value)) = body.split_once('=')
            && key.trim() == MIME_TYPE
            && value.split(';').any(|id| id.trim() == DESKTOP_ID)
        {
            changed = true;
            let rest: Vec<&str> = value.split(';').map(str::trim).filter(|id| !id.is_empty() && *id != DESKTOP_ID).collect();
            if !rest.is_empty() {
                let ending = &line[body.len()..];
                out.push_str(&format!("{}={};{ending}", key, rest.join(";")));
            }
            continue;
        }
        out.push_str(line);
    }
    changed.then_some(out)
}

/// The `Exec=` line of desktop file `id`, for the log.
fn find_exec(id: &str, apps_dir: &Path) -> Option<String> {
    if id.contains('/') {
        return None;
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    std::iter::once(apps_dir.to_path_buf())
        .chain(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("applications")))
        .find_map(|dir| std::fs::read_to_string(dir.join(id)).ok())
        .and_then(|text| text.lines().find_map(|l| l.strip_prefix("Exec=").map(str::to_owned)))
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    paths::write_atomic(path, contents, None, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeDesktop {
        default: Mutex<String>,
        calls: Mutex<Vec<String>>,
    }

    impl Desktop for FakeDesktop {
        fn query_default(&self) -> Result<String, String> {
            self.calls.lock().unwrap().push("query".into());
            Ok(self.default.lock().unwrap().clone())
        }
        fn set_default(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("set".into());
            *self.default.lock().unwrap() = DESKTOP_ID.to_owned();
            Ok(())
        }
        fn refresh(&self, _: &Path) {
            self.calls.lock().unwrap().push("refresh".into());
        }
    }

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cloudrdpapp-urlh-{}", crate::util::random_token(6)))
    }

    #[test]
    fn desktop_entry_runs_this_program_with_the_callback_flag() {
        let entry = desktop_entry(Path::new("/home/u/.local/bin/cloudrdpapp")).unwrap();
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nExec=/home/u/.local/bin/cloudrdpapp --auth-callback %u\n"));
        assert!(entry.contains("\nNoDisplay=true\n"));
        assert!(entry.contains("\nMimeType=x-scheme-handler/ms-appx-web;\n"));
        assert!(desktop_entry(Path::new("relative/cloudrdpapp")).is_err());
    }

    #[test]
    fn exec_paths_are_quoted() {
        assert_eq!(exec_quote("/a/b-c_d.e/f").unwrap(), "/a/b-c_d.e/f");
        assert_eq!(exec_quote("/my dir/app").unwrap(), "\"/my dir/app\"");
        assert_eq!(exec_quote("/100%/app").unwrap(), "/100%%/app");
        assert_eq!(exec_quote("/a$b/\"c\"").unwrap(), "\"/a\\\\$b/\\\\\"c\\\\\"\"");
        assert_eq!(exec_quote("/a\\b").unwrap(), "\"/a\\\\\\\\b\"");
        assert!(exec_quote("/a\nb").is_err());
    }

    #[test]
    fn registers_once_and_follows_the_executable() {
        let dir = test_dir();
        let desktop = FakeDesktop::default();
        let exe = Path::new("/opt/one/cloudrdpapp");
        assert_eq!(ensure_registered_in(&dir, exe, &desktop).unwrap(), Outcome::Registered { replaced: None });
        let text = std::fs::read_to_string(dir.join(DESKTOP_ID)).unwrap();
        assert!(text.contains("Exec=/opt/one/cloudrdpapp --auth-callback %u"));
        assert_eq!(*desktop.calls.lock().unwrap(), ["refresh", "query", "set"]);

        // Already pointing here: only a query.
        desktop.calls.lock().unwrap().clear();
        assert_eq!(ensure_registered_in(&dir, exe, &desktop).unwrap(), Outcome::AlreadyRegistered);
        assert_eq!(*desktop.calls.lock().unwrap(), ["query"]);

        // Another build: the entry is rewritten, the default kept.
        desktop.calls.lock().unwrap().clear();
        let other = Path::new("/src/target/debug/cloudrdpapp");
        assert_eq!(ensure_registered_in(&dir, other, &desktop).unwrap(), Outcome::Registered { replaced: None });
        assert!(std::fs::read_to_string(dir.join(DESKTOP_ID)).unwrap().contains("Exec=/src/target/debug/cloudrdpapp "));
        assert_eq!(*desktop.calls.lock().unwrap(), ["refresh", "query", "set"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_another_handler_and_reports_it() {
        let dir = test_dir();
        let desktop = FakeDesktop::default();
        *desktop.default.lock().unwrap() = "freerdp-aad-handler.desktop".to_owned();
        let outcome = ensure_registered_in(&dir, Path::new("/usr/bin/cloudrdpapp"), &desktop).unwrap();
        assert_eq!(outcome, Outcome::Registered { replaced: Some("freerdp-aad-handler.desktop".into()) });
        assert_eq!(*desktop.default.lock().unwrap(), DESKTOP_ID);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn association_removal() {
        let text = "[Default Applications]\r\ntext/html=firefox.desktop\r\nx-scheme-handler/ms-appx-web=io.github.mannsion.CloudRDPApp.AuthCallback.desktop\r\n\r\n[Added Associations]\nx-scheme-handler/ms-appx-web=other.desktop;io.github.mannsion.CloudRDPApp.AuthCallback.desktop;\n";
        let out = remove_association(text).unwrap();
        assert_eq!(
            out,
            "[Default Applications]\r\ntext/html=firefox.desktop\r\n\r\n[Added Associations]\nx-scheme-handler/ms-appx-web=other.desktop;\n"
        );
        assert_eq!(remove_association(&out), None);
        assert_eq!(remove_association("x-scheme-handler/ms-appx-web=other.desktop\n"), None);
    }

    #[test]
    fn unregister_removes_entry_and_associations() {
        let dir = test_dir();
        let desktop = FakeDesktop::default();
        ensure_registered_in(&dir, Path::new("/usr/bin/cloudrdpapp"), &desktop).unwrap();
        let list = dir.join("mimeapps.list");
        std::fs::write(&list, format!("[Default Applications]\n{MIME_TYPE}={DESKTOP_ID}\n")).unwrap();
        unregister_in(&dir, &[list.clone(), dir.join("missing.list")], &desktop).unwrap();
        assert!(!dir.join(DESKTOP_ID).exists());
        assert_eq!(std::fs::read_to_string(&list).unwrap(), "[Default Applications]\n");
        // Twice is fine.
        unregister_in(&dir, &[list], &desktop).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
