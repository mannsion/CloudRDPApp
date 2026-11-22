//! FreeRDP's SDL client reads its shortcut modifier from
//! `~/.config/freerdp/sdl-freerdp.json`; there is no command-line switch.
//!
//! By default that modifier is Right Shift, so typing a capital D with the
//! right-hand Shift key disconnects the session. This module sets a safer
//! modifier while leaving every other key in the file alone.

use std::path::Path;

use serde_json::{Map, Value};

use crate::config::HotkeyModifier;

const KEY: &str = "SDL_KeyModMask";

/// Apply the chosen modifier. Returns `Ok(true)` when the file was changed.
/// A file that exists but is not a JSON object is left untouched.
pub fn apply(modifier: HotkeyModifier, path: &Path) -> Result<bool, String> {
    let Some(mask) = modifier.sdl_mask() else {
        return Ok(false);
    };
    let mut root: Map<String, Value> = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            Ok(_) | Err(_) => return Err(format!("{} is not a JSON object; leaving it unchanged", path.display())),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };

    let wanted = Value::Array(mask.iter().map(|m| Value::String((*m).to_owned())).collect());
    if root.get(KEY) == Some(&wanted) {
        return Ok(false);
    }
    root.insert(KEY.to_owned(), wanted);

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|err| err.to_string())?;
    text.push('\n');
    crate::paths::write_atomic(path, text.as_bytes(), None, false)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("cloudrdpapp-sdl-{}", crate::util::random_token(6)))
            .join("freerdp")
            .join("sdl-freerdp.json")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_the_file_when_missing() {
        let path = temp_file();
        assert_eq!(apply(HotkeyModifier::CtrlAlt, &path), Ok(true));
        assert_eq!(read(&path)[KEY], serde_json::json!(["KMOD_LCTRL", "KMOD_LALT"]));
        // Already as wanted: nothing is rewritten.
        assert_eq!(apply(HotkeyModifier::CtrlAlt, &path), Ok(false));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn preserves_other_settings() {
        let path = temp_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"SDL_Fullscreen":"SDL_SCANCODE_F11","SDL_KeyModMask":["KMOD_RSHIFT"]}"#).unwrap();
        assert_eq!(apply(HotkeyModifier::Disabled, &path), Ok(true));
        let json = read(&path);
        assert_eq!(json["SDL_Fullscreen"], "SDL_SCANCODE_F11");
        assert_eq!(json[KEY], serde_json::json!(["KMOD_NONE"]));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn unmanaged_and_unreadable_files_are_left_alone() {
        let path = temp_file();
        assert_eq!(apply(HotkeyModifier::Unmanaged, &path), Ok(false));
        assert!(!path.exists());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "// hand-written, not strict JSON\n{}").unwrap();
        assert!(apply(HotkeyModifier::CtrlAlt, &path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// hand-written, not strict JSON\n{}");
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
