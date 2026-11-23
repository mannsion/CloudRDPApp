//! Persistent settings: saved accounts, connection defaults and per-resource
//! overrides.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::auth::Account;
use crate::paths;

/// How the remote desktop is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayMode {
    /// Fill the monitor at its native resolution.
    #[default]
    Fullscreen,
    /// A resizable window; the remote resolution follows the window.
    DynamicWindow,
    /// A window with a fixed remote resolution.
    FixedWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScaleMode {
    /// Match the local monitor's scale factor.
    #[default]
    Auto,
    /// Use `scale_percent`.
    Custom,
}

/// Floating connection bar shown at the top of an active RDP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionBar {
    /// Hidden handle at the top-center edge; hover to reveal, pin to keep open.
    #[default]
    AutoHide,
    /// Always visible (pinned) at the top-center edge while connected.
    Pinned,
    /// Disabled.
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    pub mode: DisplayMode,
    /// Span every monitor (full screen only).
    pub multi_monitor: bool,
    /// Initial window size, and the remote resolution of a fixed window.
    pub width: u32,
    pub height: u32,
    pub scale: ScaleMode,
    /// Windows display scaling in percent (100–500).
    pub scale_percent: u32,
    /// Stretch the remote image to the window instead of showing scroll bars
    /// (fixed-size windows only).
    pub smart_sizing: bool,
    /// Floating connection bar at the top of an active RDP session.
    pub connection_bar: ConnectionBar,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Fullscreen,
            multi_monitor: false,
            width: 1920,
            height: 1080,
            scale: ScaleMode::Auto,
            scale_percent: 100,
            smart_sizing: false,
            connection_bar: ConnectionBar::AutoHide,
        }
    }
}

/// Which codecs the client offers to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GraphicsCodec {
    /// H.264 with full chroma (AVC 4:4:4): sharpest text, most CPU.
    #[default]
    Avc444,
    /// H.264 4:2:0: less CPU and bandwidth, slightly soft coloured text.
    Avc420,
    /// Graphics pipeline without H.264 (RemoteFX progressive).
    Progressive,
    /// Classic bitmap updates, for troubleshooting only.
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkProfile {
    /// Let client and server measure the link and adapt.
    #[default]
    Auto,
    Lan,
    BroadbandHigh,
    Broadband,
    BroadbandLow,
    Wan,
    Modem,
}

impl NetworkProfile {
    pub fn freerdp_value(self) -> &'static str {
        match self {
            NetworkProfile::Auto => "auto",
            NetworkProfile::Lan => "lan",
            NetworkProfile::BroadbandHigh => "broadband-high",
            NetworkProfile::Broadband => "broadband",
            NetworkProfile::BroadbandLow => "broadband-low",
            NetworkProfile::Wan => "wan",
            NetworkProfile::Modem => "modem",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphicsSettings {
    pub codec: GraphicsCodec,
    pub network: NetworkProfile,
    pub wallpaper: bool,
    pub themes: bool,
    pub font_smoothing: bool,
    pub desktop_composition: bool,
    pub window_drag: bool,
    pub menu_animations: bool,
    /// Video optimised remoting channel for media playback.
    pub video_channel: bool,
}

impl Default for GraphicsSettings {
    fn default() -> Self {
        Self {
            codec: GraphicsCodec::Avc444,
            network: NetworkProfile::Auto,
            wallpaper: true,
            themes: true,
            font_smoothing: true,
            desktop_composition: true,
            window_drag: true,
            menu_animations: true,
            video_channel: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AudioPlayback {
    /// Play remote sound on this computer.
    #[default]
    Local,
    /// Leave sound playing on the remote computer.
    Remote,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AudioBackend {
    /// PulseAudio API (also served by PipeWire).
    #[default]
    Pulse,
    Alsa,
}

impl AudioBackend {
    pub fn freerdp_value(self) -> &'static str {
        match self {
            AudioBackend::Pulse => "pulse",
            AudioBackend::Alsa => "alsa",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AudioQuality {
    /// Uncompressed where the server allows it.
    #[default]
    High,
    Medium,
    /// Let the server adapt to the link.
    Dynamic,
}

impl AudioQuality {
    pub fn freerdp_value(self) -> &'static str {
        match self {
            AudioQuality::High => "high",
            AudioQuality::Medium => "medium",
            AudioQuality::Dynamic => "dynamic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub playback: AudioPlayback,
    pub backend: AudioBackend,
    pub quality: AudioQuality,
    /// Playback buffer in milliseconds; 0 keeps FreeRDP's default.
    pub latency_ms: u32,
    pub microphone: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            playback: AudioPlayback::Local,
            backend: AudioBackend::Pulse,
            quality: AudioQuality::High,
            latency_ms: 0,
            microphone: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DriveSharing {
    #[default]
    Off,
    /// Share the home folder.
    Home,
    /// Share `shared_folder`.
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    pub clipboard: bool,
    pub drives: DriveSharing,
    pub shared_folder: String,
    pub camera: bool,
    pub printers: bool,
    pub smartcards: bool,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            clipboard: true,
            drives: DriveSharing::Off,
            shared_folder: String::new(),
            camera: false,
            printers: false,
            smartcards: false,
        }
    }
}

/// Which FreeRDP front end to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    /// `sdl-freerdp3`: the actively developed client, native on Wayland.
    #[default]
    Sdl,
    /// `xfreerdp3`: the X11 client, with the most complete RemoteApp support.
    X11,
}

/// Video driver for the SDL client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VideoBackend {
    /// Let SDL pick (Wayland on a Wayland session).
    #[default]
    Auto,
    Wayland,
    /// Force X11 (XWayland on a Wayland session).
    X11,
}

/// How the session host authenticates the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HostAuth {
    /// Follow the connection file: Entra single sign-on when the host pool
    /// enables it (`enablerdsaadauth:i:1`), otherwise a password.
    #[default]
    Auto,
    /// Force Entra single sign-on (`/sec:aad`).
    EntraSso,
    /// Force username and password.
    Password,
}

/// How FreeRDP's connection-time sign-in is satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectAuth {
    /// Mint the tokens from the saved sign-in; no browser window.
    #[default]
    Silent,
    /// Send every token request through the system browser.
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CertPolicy {
    /// Ask in a FreeRDP dialog when a certificate cannot be verified.
    #[default]
    Ask,
    /// Accept on first use, refuse if it later changes.
    TrustOnFirstUse,
    /// Never verify. Unsafe outside a trusted network.
    Ignore,
}

/// Modifier for the SDL client's own shortcuts (… + Enter toggles full
/// screen, + D disconnects, + M minimises, + G toggles the keyboard grab).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyModifier {
    /// Right Ctrl. Leaves Ctrl + Alt free for Windows and avoids desktop
    /// shortcut conflicts.
    #[default]
    RightCtrl,
    /// Ctrl + Alt, the classic RDP convention. Cannot collide with typing.
    CtrlAlt,
    /// FreeRDP's default. Typing a capital D with Right Shift disconnects.
    RightShift,
    RightCtrlRightShift,
    Disabled,
    /// Leave FreeRDP's configuration file untouched.
    Unmanaged,
}

impl HotkeyModifier {
    /// The `SDL_KeyModMask` value, or `None` to leave the file alone.
    pub fn sdl_mask(self) -> Option<&'static [&'static str]> {
        match self {
            HotkeyModifier::RightCtrl => Some(&["KMOD_RCTRL"]),
            HotkeyModifier::CtrlAlt => Some(&["KMOD_LCTRL", "KMOD_LALT"]),
            HotkeyModifier::RightShift => Some(&["KMOD_RSHIFT"]),
            HotkeyModifier::RightCtrlRightShift => Some(&["KMOD_RCTRL", "KMOD_RSHIFT"]),
            HotkeyModifier::Disabled => Some(&["KMOD_NONE"]),
            HotkeyModifier::Unmanaged => None,
        }
    }

    /// Human-readable shortcut modifier name for UI hints, if active.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            HotkeyModifier::RightCtrl => Some("Right Ctrl"),
            HotkeyModifier::CtrlAlt => Some("Ctrl + Alt"),
            HotkeyModifier::RightShift => Some("Right Shift"),
            HotkeyModifier::RightCtrlRightShift => Some("Right Ctrl + Right Shift"),
            HotkeyModifier::Disabled | HotkeyModifier::Unmanaged => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn freerdp_value(self) -> &'static str {
        match self {
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedSettings {
    pub client: ClientKind,
    /// Use the X11 client for RemoteApps even when `client` is SDL.
    pub remote_apps_use_x11: bool,
    pub video_backend: VideoBackend,
    pub host_auth: HostAuth,
    pub connect_auth: ConnectAuth,
    pub certificates: CertPolicy,
    pub hotkeys: HotkeyModifier,
    /// Send every key, including the Super key and Alt+Tab, to the remote.
    pub grab_keyboard: bool,
    pub auto_reconnect: bool,
    pub log_level: LogLevel,
    /// Extra FreeRDP arguments, appended last so they win.
    pub extra_args: String,
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self {
            client: ClientKind::Sdl,
            remote_apps_use_x11: true,
            video_backend: VideoBackend::Auto,
            host_auth: HostAuth::Auto,
            connect_auth: ConnectAuth::Silent,
            certificates: CertPolicy::Ask,
            hotkeys: HotkeyModifier::RightCtrl,
            grab_keyboard: true,
            auto_reconnect: true,
            log_level: LogLevel::Info,
            extra_args: String::new(),
        }
    }
}

/// Everything that shapes one connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConnectionSettings {
    pub display: DisplaySettings,
    pub graphics: GraphicsSettings,
    pub audio: AudioSettings,
    pub devices: DeviceSettings,
    pub advanced: AdvancedSettings,
}

/// The configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// The account to restore at start-up.
    pub account: Option<Account>,
    /// Settings used by every resource without an override.
    pub defaults: ConnectionSettings,
    /// Per-resource settings, keyed by [`Resource::key`](crate::feed::Resource::key).
    pub overrides: BTreeMap<String, ConnectionSettings>,
    /// Paths of the FreeRDP binaries, when not found on `PATH`.
    pub sdl_client_path: String,
    pub x11_client_path: String,
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&paths::config_file())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(config) => config,
                Err(err) => {
                    tracing::warn!("ignoring unreadable settings file {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&paths::config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|err| err.to_string())?;
        paths::write_private_file(path, &json).map_err(|err| format!("{}: {err}", path.display()))
    }

    /// The settings that apply to a resource.
    pub fn settings_for(&self, resource_key: &str) -> &ConnectionSettings {
        self.overrides.get(resource_key).unwrap_or(&self.defaults)
    }

    pub fn has_override(&self, resource_key: &str) -> bool {
        self.overrides.contains_key(resource_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_favour_quality() {
        let s = ConnectionSettings::default();
        assert_eq!(s.display.mode, DisplayMode::Fullscreen);
        assert_eq!(s.display.scale, ScaleMode::Auto);
        assert_eq!(s.display.connection_bar, ConnectionBar::AutoHide);
        assert_eq!(s.graphics.codec, GraphicsCodec::Avc444);
        assert_eq!(s.graphics.network, NetworkProfile::Auto);
        assert!(s.graphics.font_smoothing && s.graphics.wallpaper);
        assert_eq!(s.audio.playback, AudioPlayback::Local);
        assert_eq!(s.audio.quality, AudioQuality::High);
        assert!(s.audio.microphone);
        assert_eq!(s.advanced.certificates, CertPolicy::Ask);
        assert_eq!(s.advanced.connect_auth, ConnectAuth::Silent);
        assert_eq!(s.advanced.hotkeys, HotkeyModifier::RightCtrl);
        assert_eq!(HotkeyModifier::RightCtrl.sdl_mask(), Some(&["KMOD_RCTRL"][..]));
        assert_eq!(HotkeyModifier::RightCtrl.hint(), Some("Right Ctrl"));
        assert_eq!(HotkeyModifier::Disabled.hint(), None);
        assert_eq!(HotkeyModifier::Unmanaged.hint(), None);
    }

    #[test]
    fn roundtrips_through_json() {
        let dir = std::env::temp_dir().join(format!("cloudrdpapp-cfg-{}", crate::util::random_token(6)));
        let path = dir.join("config.json");
        let mut config = Config {
            account: Some(Account {
                tenant_id: "t".into(),
                object_id: "o".into(),
                username: "u@x.io".into(),
                name: Some("U".into()),
            }),
            ..Config::default()
        };
        config.defaults.display.mode = DisplayMode::DynamicWindow;
        config.defaults.display.connection_bar = ConnectionBar::Pinned;
        let mut custom = ConnectionSettings::default();
        custom.display.connection_bar = ConnectionBar::Off;
        custom.graphics.codec = GraphicsCodec::Avc420;
        custom.advanced.hotkeys = HotkeyModifier::CtrlAlt;
        custom.advanced.extra_args = "/kbd:layout:0x409".into();
        config.overrides.insert("abc".into(), custom.clone());
        config.save_to(&path).unwrap();

        let loaded = Config::load_from(&path);
        assert_eq!(loaded, config);
        assert_eq!(loaded.settings_for("abc"), &custom);
        assert_eq!(loaded.settings_for("other").display.mode, DisplayMode::DynamicWindow);
        assert_eq!(loaded.settings_for("other").display.connection_bar, ConnectionBar::Pinned);
        assert!(loaded.has_override("abc") && !loaded.has_override("other"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let json = r#"{"defaults":{"display":{"mode":"fixed-window","future_field":1},"graphics":{}},"new_section":true}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.defaults.display.mode, DisplayMode::FixedWindow);
        assert_eq!(config.defaults.display.width, 1920);
        assert_eq!(config.defaults.display.connection_bar, ConnectionBar::AutoHide);
        assert_eq!(config.defaults.graphics, GraphicsSettings::default());
        assert_eq!(config.defaults.advanced.hotkeys, HotkeyModifier::RightCtrl);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join(format!("cloudrdpapp-bad-{}.json", crate::util::random_token(6)));
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Config::load_from(&path), Config::default());
        std::fs::remove_file(&path).unwrap();
    }
}
