//! Turning settings into a FreeRDP invocation.
//!
//! The arguments are handed to FreeRDP on a private pipe
//! (`/args-from:fd:3`, one per line), never on the command line, so a
//! password cannot be read from `/proc/<pid>/cmdline`.

use std::path::{Path, PathBuf};

use super::file::RdpFile;
use crate::config::{
    AudioPlayback, CertPolicy, ClientKind, ConnectionSettings, DisplayMode, DriveSharing, GraphicsCodec,
    HostAuth, ScaleMode, VideoBackend,
};
use crate::util;

/// How FreeRDP obtains its connection-time tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRoute {
    /// Point FreeRDP's Entra authority at the app's local token relay
    /// (`host:port`), which mints tokens from the saved sign-in.
    Relay { authority: String },
    /// Leave FreeRDP talking to Entra ID with its default broker redirect
    /// (`ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>`, the
    /// `avd-token` format) and capture the browser's return through
    /// `--auth-callback`.
    Browser,
}

/// Everything needed to plan a launch.
#[derive(Debug, Clone)]
pub struct LaunchInput<'a> {
    pub settings: &'a ConnectionSettings,
    /// The connection file as downloaded from the feed.
    pub rdp: RdpFile,
    pub client: ClientKind,
    pub program: PathBuf,
    /// User principal name of the signed-in account.
    pub username: &'a str,
    /// Password for hosts without Entra single sign-on.
    pub password: Option<&'a str>,
    pub title: &'a str,
    /// This executable, which FreeRDP spawns as its sign-in helper.
    pub helper_exe: &'a Path,
    pub token_route: TokenRoute,
    /// Scale of the local monitor in percent, when known.
    pub local_scale_percent: Option<u32>,
}

/// A planned FreeRDP launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    /// Arguments that follow the connection file path.
    pub args: Vec<String>,
    /// The connection file with conflicting properties adjusted.
    pub rdp: RdpFile,
    pub env: Vec<(String, String)>,
    /// True when the session host is signed in to with an Entra token.
    pub entra_sso: bool,
    pub remote_app: bool,
}

/// Decide whether the session host is signed in to with Entra single sign-on.
pub fn uses_entra_sso(settings: &ConnectionSettings, rdp: &RdpFile) -> bool {
    match settings.advanced.host_auth {
        HostAuth::Auto => rdp.offers_entra_sso(),
        HostAuth::EntraSso => true,
        HostAuth::Password => false,
    }
}

/// Pick the client for a resource: RemoteApps go to the X11 client when
/// that is enabled and available, because its window integration is the
/// complete one.
pub fn choose_client(settings: &ConnectionSettings, remote_app: bool, x11_available: bool) -> ClientKind {
    if remote_app && settings.advanced.remote_apps_use_x11 && x11_available {
        ClientKind::X11
    } else {
        settings.advanced.client
    }
}

pub fn build(input: LaunchInput<'_>) -> Result<LaunchPlan, String> {
    let LaunchInput { settings, mut rdp, client, program, username, password, title, helper_exe, token_route, local_scale_percent } =
        input;
    let remote_app = rdp.is_remote_app();
    let entra_sso = uses_entra_sso(settings, &rdp);
    let mut args: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();

    // --- gateway and sign-in -------------------------------------------
    let is_avd = rdp.is_avd();
    if is_avd {
        args.push("/gateway:type:arm".into());
    }
    if is_avd || entra_sso {
        let helper = helper_exe.to_str().ok_or("the application path is not valid UTF-8")?;
        if helper.contains(',') {
            return Err("the application is installed in a folder with a comma in its name, which FreeRDP cannot handle".into());
        }
        let azure = match &token_route {
            TokenRoute::Relay { authority } => format!("ad:{authority},auth-helper:{helper}"),
            // With a helper FreeRDP uses `avd-token` (default: the broker
            // redirect) for both gateway and session-host requests.
            TokenRoute::Browser => format!("auth-helper:{helper}"),
        };
        args.push(format!("/azure:{azure}"));
    }

    if entra_sso {
        args.push("/sec:aad".into());
    }
    if !username.is_empty() {
        args.push(format!("/u:{username}"));
    }
    if !entra_sso {
        if let Some(password) = password {
            if password.contains(['\n', '\r']) {
                return Err("the password contains a line break".into());
            }
            args.push(format!("/p:{password}"));
        }
        // An Entra account signs in with its full user name and no domain.
        args.push("/d:".into());
    }
    let clean_title: String = title.chars().filter(|c| !c.is_control()).collect();
    if !clean_title.trim().is_empty() {
        args.push(format!("/t:{}", clean_title.trim()));
    }

    // --- display -----------------------------------------------------------
    // The feed's file may carry display properties that win over the command
    // line (notably `use multimon`), so the file is brought in line too.
    for name in ["desktopwidth", "desktopheight", "desktop size id", "dynamic resolution", "span monitors", "selectedmonitors", "maximizetocurrentdisplays", "singlemoninwindowedmode"] {
        rdp.remove(name);
    }
    let display = &settings.display;
    let size = format!("/size:{}x{}", display.width.clamp(640, 16384), display.height.clamp(480, 16384));
    if remote_app {
        rdp.remove("smart sizing");
        rdp.set_int("use multimon", 0);
    } else {
        match display.mode {
            DisplayMode::Fullscreen => {
                args.push("/f".into());
                rdp.set_int("screen mode id", 2);
                rdp.set_int("smart sizing", 0);
                rdp.set_int("use multimon", i64::from(display.multi_monitor));
                if display.multi_monitor {
                    args.push("/multimon".into());
                }
            }
            DisplayMode::DynamicWindow => {
                // Smart sizing and dynamic resolution are mutually exclusive.
                args.push("/dynamic-resolution".into());
                args.push(size);
                rdp.set_int("screen mode id", 1);
                rdp.set_int("smart sizing", 0);
                rdp.set_int("use multimon", 0);
            }
            DisplayMode::FixedWindow => {
                args.push(size);
                rdp.set_int("screen mode id", 1);
                rdp.set_int("smart sizing", i64::from(display.smart_sizing));
                rdp.set_int("use multimon", 0);
                if display.smart_sizing {
                    args.push("/smart-sizing".into());
                }
            }
        }
    }

    let scale = match display.scale {
        ScaleMode::Custom => Some(display.scale_percent),
        // The SDL client reads each monitor's scale itself; the X11 client
        // needs to be told.
        ScaleMode::Auto if client == ClientKind::X11 => local_scale_percent.filter(|p| *p != 100),
        ScaleMode::Auto => None,
    };
    if let Some(percent) = scale {
        let percent = percent.clamp(100, 500);
        args.push(format!("/scale-desktop:{percent}"));
        args.push(format!("/scale-device:{}", nearest_device_scale(percent)));
    }

    // --- graphics ----------------------------------------------------------
    args.push(format!("/network:{}", settings.graphics.network.freerdp_value()));
    args.push(
        match settings.graphics.codec {
            GraphicsCodec::Avc444 => "/gfx:AVC444:on,AVC420:on,RFX:on",
            GraphicsCodec::Avc420 => "/gfx:AVC420:on,RFX:on",
            GraphicsCodec::Progressive => "/gfx:RFX:on,progressive:on",
            GraphicsCodec::Legacy => "-gfx",
        }
        .into(),
    );
    args.push("/bpp:32".into());
    let g = &settings.graphics;
    for (enabled, name) in [
        (g.wallpaper, "wallpaper"),
        (g.themes, "themes"),
        (g.font_smoothing, "fonts"),
        (g.desktop_composition, "aero"),
        (g.window_drag, "window-drag"),
        (g.menu_animations, "menu-anims"),
    ] {
        args.push(format!("{}{name}", if enabled { '+' } else { '-' }));
    }
    if g.video_channel {
        args.push("+video".into());
        args.push("+geometry".into());
    }

    // --- audio -------------------------------------------------------------
    let audio = &settings.audio;
    let backend = audio.backend.freerdp_value();
    match audio.playback {
        AudioPlayback::Local => {
            let mut sound = format!("/sound:sys:{backend},quality:{}", audio.quality.freerdp_value());
            if audio.latency_ms > 0 {
                sound.push_str(&format!(",latency:{}", audio.latency_ms.min(2000)));
            }
            args.push(sound);
            args.push("/audio-mode:0".into());
        }
        AudioPlayback::Remote => args.push("/audio-mode:1".into()),
        AudioPlayback::Off => args.push("/audio-mode:2".into()),
    }
    if audio.microphone {
        args.push(format!("/microphone:sys:{backend}"));
    }

    // --- local devices -----------------------------------------------------
    let devices = &settings.devices;
    args.push(if devices.clipboard { "+clipboard" } else { "-clipboard" }.into());
    match devices.drives {
        DriveSharing::Off => {}
        DriveSharing::Home => {
            args.push("+home-drive".into());
            rdp.set_str("drivestoredirect", "*");
        }
        DriveSharing::Folder => {
            let folder = devices.shared_folder.trim();
            if folder.is_empty() {
                return Err("folder sharing is on but no folder is chosen".into());
            }
            if folder.contains(',') || folder.chars().any(char::is_control) {
                return Err("the shared folder path must not contain commas or control characters".into());
            }
            if !Path::new(folder).is_dir() {
                return Err(format!("the shared folder {folder} does not exist"));
            }
            args.push(format!("/drive:Shared,{folder}"));
            rdp.set_str("drivestoredirect", "*");
        }
    }
    if devices.camera {
        args.push("/dvc:rdpecam".into());
    }
    if devices.printers {
        args.push("/printer".into());
    }
    if devices.smartcards {
        args.push("/smartcard".into());
    }

    // --- behaviour ---------------------------------------------------------
    let advanced = &settings.advanced;
    args.push(if advanced.grab_keyboard { "+grab-keyboard" } else { "-grab-keyboard" }.into());
    match (advanced.certificates, client) {
        // The SDL client asks in a dialog. The X11 client would ask on a
        // terminal that does not exist here, so it pins on first use instead.
        (CertPolicy::Ask, ClientKind::Sdl) => {}
        (CertPolicy::Ask, ClientKind::X11) | (CertPolicy::TrustOnFirstUse, _) => args.push("/cert:tofu".into()),
        (CertPolicy::Ignore, _) => args.push("/cert:ignore".into()),
    }
    if advanced.auto_reconnect {
        args.push("+auto-reconnect".into());
        args.push("/auto-reconnect-max-retries:20".into());
    }
    args.push(format!("/log-level:{}", advanced.log_level.freerdp_value()));

    if client == ClientKind::Sdl {
        match advanced.video_backend {
            VideoBackend::Auto => {}
            VideoBackend::Wayland => env.push(("SDL_VIDEODRIVER".into(), "wayland".into())),
            VideoBackend::X11 => env.push(("SDL_VIDEODRIVER".into(), "x11".into())),
        }
    }

    // --- user extras, last so they override ---------------------------------
    let extra = util::shell_split(&advanced.extra_args).map_err(|err| format!("extra arguments: {err}"))?;
    for arg in extra {
        if arg.contains(['\n', '\r']) {
            return Err("extra arguments must not contain line breaks".into());
        }
        if arg.starts_with("/args-from") {
            return Err("/args-from cannot be used in extra arguments".into());
        }
        args.push(arg);
    }

    if args.iter().any(|arg| arg.is_empty()) {
        return Err("an empty argument would end FreeRDP's argument list early".into());
    }
    Ok(LaunchPlan { program, args, rdp, env, entra_sso, remote_app })
}

/// Windows accepts 100, 140 or 180 for the device (UWP) scale factor.
fn nearest_device_scale(desktop_percent: u32) -> u32 {
    match desktop_percent {
        0..=119 => 100,
        120..=159 => 140,
        _ => 180,
    }
}

/// The text written to FreeRDP's argument pipe: the connection file first,
/// then one argument per line. A blank line ends the list.
pub fn args_payload(rdp_path: &Path, args: &[String]) -> Result<String, String> {
    let path = rdp_path.to_str().ok_or("the connection file path is not valid UTF-8")?;
    if path.contains(['\n', '\r', '\0']) {
        return Err("the connection file path contains invalid control characters".into());
    }
    let mut out = String::with_capacity(256 + args.iter().map(|a| a.len() + 1).sum::<usize>());
    out.push_str(path);
    out.push('\n');
    for arg in args {
        if arg.contains(['\n', '\r', '\0']) {
            return Err("a launch argument contains invalid control characters".into());
        }
        out.push_str(arg);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioBackend, AudioQuality, HotkeyModifier, LogLevel, NetworkProfile};
    use crate::rdp::file::SAMPLE_AVD_RDP;

    fn input<'a>(settings: &'a ConnectionSettings, rdp: &str) -> LaunchInput<'a> {
        LaunchInput {
            settings,
            rdp: RdpFile::parse(rdp),
            client: ClientKind::Sdl,
            program: "/usr/bin/sdl-freerdp3".into(),
            username: "alex@contoso.com",
            password: None,
            title: "Dev Desktop",
            helper_exe: Path::new("/usr/bin/cloudrdpapp"),
            token_route: TokenRoute::Relay { authority: "127.0.0.1:45123".into() },
            local_scale_percent: Some(145),
        }
    }

    fn has(plan: &LaunchPlan, arg: &str) -> bool {
        plan.args.iter().any(|a| a == arg)
    }

    #[test]
    fn default_plan_for_an_sso_desktop() {
        let settings = ConnectionSettings::default();
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        assert!(plan.entra_sso);
        assert!(!plan.remote_app);
        assert_eq!(plan.args[0], "/gateway:type:arm");
        assert_eq!(plan.args[1], "/azure:ad:127.0.0.1:45123,auth-helper:/usr/bin/cloudrdpapp");
        for expected in [
            "/sec:aad",
            "/u:alex@contoso.com",
            "/t:Dev Desktop",
            "/f",
            "/network:auto",
            "/gfx:AVC444:on,AVC420:on,RFX:on",
            "+fonts",
            "+wallpaper",
            "/sound:sys:pulse,quality:high",
            "/audio-mode:0",
            "/microphone:sys:pulse",
            "+clipboard",
            "+grab-keyboard",
            "+auto-reconnect",
            "/log-level:INFO",
        ] {
            assert!(has(&plan, expected), "missing {expected} in {:?}", plan.args);
        }
        // No password, no certificate override, no manual scale with the SDL client.
        assert!(!plan.args.iter().any(|a| a.starts_with("/p:") || a.starts_with("/cert") || a.starts_with("/scale")));
        assert!(!has(&plan, "/multimon"));
        // The feed asked for multimon and smart sizing; the file is corrected.
        assert_eq!(plan.rdp.get_int("use multimon"), Some(0));
        assert_eq!(plan.rdp.get_int("smart sizing"), Some(0));
        assert_eq!(plan.rdp.get_int("screen mode id"), Some(2));
        // Routing properties are untouched.
        assert_eq!(plan.rdp.get("loadbalanceinfo"), Some("mth://localhost/11111111-2222/33333333-4444"));
        assert!(plan.env.is_empty());
    }

    #[test]
    fn dynamic_window_and_custom_scale() {
        let mut settings = ConnectionSettings::default();
        settings.display.mode = DisplayMode::DynamicWindow;
        settings.display.width = 2560;
        settings.display.height = 1440;
        settings.display.scale = ScaleMode::Custom;
        settings.display.scale_percent = 150;
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        assert!(has(&plan, "/dynamic-resolution") && has(&plan, "/size:2560x1440"));
        assert!(!has(&plan, "/f"));
        assert!(has(&plan, "/scale-desktop:150") && has(&plan, "/scale-device:140"));
        assert_eq!(plan.rdp.get_int("screen mode id"), Some(1));
    }

    #[test]
    fn multi_monitor_keeps_the_file_in_step() {
        let mut settings = ConnectionSettings::default();
        settings.display.multi_monitor = true;
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        assert!(has(&plan, "/multimon"));
        assert_eq!(plan.rdp.get_int("use multimon"), Some(1));
    }

    #[test]
    fn x11_client_gets_the_local_scale_and_pins_certificates() {
        let settings = ConnectionSettings::default();
        let mut inp = input(&settings, SAMPLE_AVD_RDP);
        inp.client = ClientKind::X11;
        let plan = build(inp).unwrap();
        assert!(has(&plan, "/scale-desktop:145") && has(&plan, "/scale-device:140"));
        assert!(has(&plan, "/cert:tofu"));
    }

    #[test]
    fn password_hosts() {
        let settings = ConnectionSettings::default();
        let rdp = SAMPLE_AVD_RDP.replace("enablerdsaadauth:i:1", "enablerdsaadauth:i:0");
        let mut inp = input(&settings, &rdp);
        inp.password = Some("hunter2 with spaces");
        let plan = build(inp).unwrap();
        assert!(!plan.entra_sso);
        assert!(!has(&plan, "/sec:aad"));
        assert!(has(&plan, "/p:hunter2 with spaces") && has(&plan, "/d:"));

        let mut bad = input(&settings, &rdp);
        bad.password = Some("two\nlines");
        assert!(build(bad).is_err());

        // Forcing SSO overrides the file.
        let mut forced = ConnectionSettings::default();
        forced.advanced.host_auth = HostAuth::EntraSso;
        assert!(build(input(&forced, &rdp)).unwrap().entra_sso);
    }

    #[test]
    fn remote_apps_skip_desktop_display_flags() {
        let settings = ConnectionSettings::default();
        let rdp = format!("{SAMPLE_AVD_RDP}remoteapplicationmode:i:1\r\nremoteapplicationprogram:s:||excel\r\n");
        let plan = build(input(&settings, &rdp)).unwrap();
        assert!(plan.remote_app);
        assert!(!has(&plan, "/f") && !plan.args.iter().any(|a| a.starts_with("/size")));
        assert_eq!(plan.rdp.get("remoteapplicationprogram"), Some("||excel"));
        assert_eq!(choose_client(&settings, true, true), ClientKind::X11);
        assert_eq!(choose_client(&settings, true, false), ClientKind::Sdl);
        assert_eq!(choose_client(&settings, false, true), ClientKind::Sdl);
    }

    #[test]
    fn browser_route_keeps_freerdps_broker_redirect() {
        let settings = ConnectionSettings::default();
        let mut inp = input(&settings, SAMPLE_AVD_RDP);
        inp.token_route = TokenRoute::Browser;
        let plan = build(inp).unwrap();
        assert_eq!(plan.args[1], "/azure:auth-helper:/usr/bin/cloudrdpapp");
        assert!(!plan.args.iter().any(|a| a.contains("avd-token") || a.contains("avd-access") || a.contains("localhost")));
    }

    #[test]
    fn graphics_audio_and_device_options() {
        let mut settings = ConnectionSettings::default();
        settings.graphics.codec = GraphicsCodec::Avc420;
        settings.graphics.network = NetworkProfile::Lan;
        settings.graphics.wallpaper = false;
        settings.graphics.video_channel = true;
        settings.audio.backend = AudioBackend::Alsa;
        settings.audio.quality = AudioQuality::Dynamic;
        settings.audio.latency_ms = 60;
        settings.audio.microphone = false;
        settings.devices.clipboard = false;
        settings.devices.drives = DriveSharing::Home;
        settings.devices.camera = true;
        settings.advanced.certificates = CertPolicy::Ignore;
        settings.advanced.auto_reconnect = false;
        settings.advanced.grab_keyboard = false;
        settings.advanced.log_level = LogLevel::Debug;
        settings.advanced.video_backend = VideoBackend::X11;
        settings.advanced.hotkeys = HotkeyModifier::Disabled;
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        for expected in [
            "/gfx:AVC420:on,RFX:on",
            "/network:lan",
            "-wallpaper",
            "+video",
            "/sound:sys:alsa,quality:dynamic,latency:60",
            "-clipboard",
            "+home-drive",
            "/dvc:rdpecam",
            "/cert:ignore",
            "-grab-keyboard",
            "/log-level:DEBUG",
        ] {
            assert!(has(&plan, expected), "missing {expected}");
        }
        assert!(!plan.args.iter().any(|a| a.starts_with("/microphone") || a == "+auto-reconnect"));
        assert_eq!(plan.rdp.get("drivestoredirect"), Some("*"));
        assert_eq!(plan.env, vec![("SDL_VIDEODRIVER".to_owned(), "x11".to_owned())]);

        settings.audio.playback = AudioPlayback::Off;
        settings.graphics.codec = GraphicsCodec::Legacy;
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        assert!(has(&plan, "/audio-mode:2") && has(&plan, "-gfx"));
        assert!(!plan.args.iter().any(|a| a.starts_with("/sound")));
    }

    #[test]
    fn plain_non_avd_rdp_skips_arm_gateway_and_azure_args() {
        let settings = ConnectionSettings::default();
        let mut inp = input(&settings, "full address:s:server.example\r\nenablerdsaadauth:i:0\r\n");
        inp.password = Some("pw");
        let plan = build(inp).unwrap();
        assert!(!plan.entra_sso);
        assert!(!plan.args.iter().any(|a| a.starts_with("/gateway:type:arm") || a.starts_with("/azure:")));
        assert!(has(&plan, "/p:pw"));
    }

    #[test]
    fn shared_folder_validation() {
        let mut settings = ConnectionSettings::default();
        settings.devices.drives = DriveSharing::Folder;
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).unwrap_err().contains("no folder"));
        settings.devices.shared_folder = "/definitely/not/here".into();
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).unwrap_err().contains("does not exist"));
        settings.devices.shared_folder = "/tmp\r/evil".into();
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).unwrap_err().contains("control"));
        settings.devices.shared_folder = "/tmp".into();
        assert!(has(&build(input(&settings, SAMPLE_AVD_RDP)).unwrap(), "/drive:Shared,/tmp"));
    }

    #[test]
    fn extra_arguments_come_last_and_are_checked() {
        let mut settings = ConnectionSettings::default();
        settings.advanced.extra_args = "/kbd:layout:0x409 '/drive:data,/mnt/my data'".into();
        let plan = build(input(&settings, SAMPLE_AVD_RDP)).unwrap();
        let n = plan.args.len();
        assert_eq!(&plan.args[n - 2..], ["/kbd:layout:0x409", "/drive:data,/mnt/my data"]);

        settings.advanced.extra_args = "/args-from:stdin".into();
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).is_err());
        settings.advanced.extra_args = "'unterminated".into();
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).is_err());
        settings.advanced.extra_args = "''".into();
        assert!(build(input(&settings, SAMPLE_AVD_RDP)).unwrap_err().contains("empty argument"));
    }

    #[test]
    fn args_payload_lists_one_argument_per_line() {
        let payload = args_payload(Path::new("/run/user/1000/x/connection.rdp"), &["/f".into(), "/u:a b".into()]).unwrap();
        assert_eq!(payload, "/run/user/1000/x/connection.rdp\n/f\n/u:a b\n");

        assert!(args_payload(Path::new("/tmp/bad\n.rdp"), &[]).is_err());
        assert!(args_payload(Path::new("/tmp/ok.rdp"), &["/u:a\nb".into()]).is_err());
        assert!(args_payload(Path::new("/tmp/ok.rdp"), &["/u:a\rb".into()]).is_err());
        assert!(args_payload(Path::new("/tmp/ok.rdp"), &["/u:a\0b".into()]).is_err());
    }

    #[test]
    fn device_scale_steps() {
        assert_eq!(nearest_device_scale(100), 100);
        assert_eq!(nearest_device_scale(125), 140);
        assert_eq!(nearest_device_scale(145), 140);
        assert_eq!(nearest_device_scale(200), 180);
    }
}
