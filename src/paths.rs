//! Locations on disk, following the XDG base directory specification.

use std::path::{Path, PathBuf};

use crate::consts::APP_DIR;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    match std::env::var_os(var) {
        Some(value) if !value.is_empty() && Path::new(&value).is_absolute() => PathBuf::from(value),
        _ => fallback.iter().fold(home(), |acc, part| acc.join(part)),
    }
}

/// `~/.config/cloudrdpapp`
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"]).join(APP_DIR)
}

/// `~/.cache/cloudrdpapp`
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", &[".cache"]).join(APP_DIR)
}

/// `~/.local/share/cloudrdpapp`
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", &[".local", "share"]).join(APP_DIR)
}

/// `~/.local/share/applications`, where per-user desktop entries live.
pub fn applications_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", &[".local", "share"]).join("applications")
}

/// `~/.config/mimeapps.list`, the per-user default applications.
pub fn mimeapps_list() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"]).join("mimeapps.list")
}

/// `~/.local/state/cloudrdpapp`
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", &[".local", "state"]).join(APP_DIR)
}

/// Per-login runtime directory (`/run/user/<uid>/cloudrdpapp`). Falls back to a
/// private directory under the cache dir when `XDG_RUNTIME_DIR` is unset.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value).join(APP_DIR),
        _ => cache_dir().join("runtime"),
    }
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

/// FreeRDP's own SDL client configuration file.
pub fn freerdp_sdl_config() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"]).join("freerdp").join("sdl-freerdp.json")
}

/// Create a directory (and parents) that only the current user can access.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    // `mode` only applies to directories that were created; tighten an
    // existing one as well.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Write `contents` to a unique temp file in the same directory and rename it
/// over `path`, removing the temp file on failure.
pub(crate) fn write_atomic(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
    sync: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file name"))?
        .to_os_string();
    tmp_name.push(format!(".tmp-{}-{}", std::process::id(), crate::util::random_token(6)));
    let tmp = path.with_file_name(tmp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        options.mode(mode);
    }
    let mut file = options.open(&tmp)?;
    let write_res = (|| {
        file.write_all(contents)?;
        if sync {
            file.sync_all()?;
        }
        Ok(())
    })();
    drop(file);
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Write a file readable only by the current user, replacing it atomically.
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_private_dir(parent)?;
    }
    write_atomic(path, contents, Some(0o600), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cloudrdpapp-test-{}", crate::util::random_token(6)));
        let nested = dir.join("nested");
        let file = nested.join("secret.txt");
        write_private_file(&file, b"one").unwrap();
        write_private_file(&file, b"two").unwrap();
        assert_eq!(std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
