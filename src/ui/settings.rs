//! The settings dialog: one `adw::PreferencesDialog` for the defaults and
//! for a single resource's override. Every change is reported at once so
//! the caller can save it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;

use crate::backend::Clients;
use crate::config::*;
use crate::rdp::freerdp;

type Shared = Rc<RefCell<ConnectionSettings>>;
type OnChange = Rc<dyn Fn(ConnectionSettings)>;
type FlushList = Rc<RefCell<Vec<Box<dyn FnOnce()>>>>;

/// What the dialog edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsTarget {
    Defaults,
    Resource { key: String, title: String },
}

/// Everything the dialog needs besides the settings themselves.
pub struct SettingsContext {
    pub target: SettingsTarget,
    /// Scale of the monitor the main window is on, in percent.
    pub local_scale: Option<u32>,
    pub clients: Clients,
    /// FreeRDP program paths (defaults dialog only).
    pub client_paths: Option<(String, String)>,
    /// Called with both paths when either changes.
    pub on_paths: Rc<dyn Fn(String, String)>,
}

const DISPLAY_MODES: &[(DisplayMode, &str)] = &[
    (DisplayMode::Fullscreen, "Full screen"),
    (DisplayMode::DynamicWindow, "Resizable window"),
    (DisplayMode::FixedWindow, "Fixed-size window"),
];
const SCALE_MODES: &[(ScaleMode, &str)] = &[(ScaleMode::Auto, "Automatic"), (ScaleMode::Custom, "Custom")];
const CONNECTION_BAR: &[(ConnectionBar, &str)] = &[
    (ConnectionBar::AutoHide, "Auto-hide (top edge)"),
    (ConnectionBar::Pinned, "Always visible"),
    (ConnectionBar::Off, "Off"),
];
const CODECS: &[(GraphicsCodec, &str)] = &[
    (GraphicsCodec::Avc444, "AVC 4:4:4"),
    (GraphicsCodec::Avc420, "AVC 4:2:0"),
    (GraphicsCodec::Progressive, "RemoteFX"),
    (GraphicsCodec::Legacy, "Legacy bitmaps"),
];
const NETWORKS: &[(NetworkProfile, &str)] = &[
    (NetworkProfile::Auto, "Detect automatically"),
    (NetworkProfile::Lan, "LAN"),
    (NetworkProfile::BroadbandHigh, "Broadband, high"),
    (NetworkProfile::Broadband, "Broadband"),
    (NetworkProfile::BroadbandLow, "Broadband, low"),
    (NetworkProfile::Wan, "WAN"),
    (NetworkProfile::Modem, "Modem"),
];
const PLAYBACK: &[(AudioPlayback, &str)] = &[
    (AudioPlayback::Local, "On this computer"),
    (AudioPlayback::Remote, "On the remote computer"),
    (AudioPlayback::Off, "Off"),
];
const AUDIO_BACKENDS: &[(AudioBackend, &str)] =
    &[(AudioBackend::Pulse, "PulseAudio"), (AudioBackend::Alsa, "ALSA")];
const AUDIO_QUALITY: &[(AudioQuality, &str)] =
    &[(AudioQuality::High, "High"), (AudioQuality::Medium, "Medium"), (AudioQuality::Dynamic, "Dynamic")];
const DRIVES: &[(DriveSharing, &str)] =
    &[(DriveSharing::Off, "Off"), (DriveSharing::Home, "Home folder"), (DriveSharing::Folder, "A chosen folder")];
const CLIENTS: &[(ClientKind, &str)] =
    &[(ClientKind::Sdl, "SDL"), (ClientKind::X11, "X11")];
const VIDEO_BACKENDS: &[(VideoBackend, &str)] =
    &[(VideoBackend::Auto, "Automatic"), (VideoBackend::Wayland, "Wayland"), (VideoBackend::X11, "X11 (XWayland)")];
const HOST_AUTH: &[(HostAuth, &str)] = &[
    (HostAuth::Auto, "Automatic"),
    (HostAuth::EntraSso, "Single sign-on"),
    (HostAuth::Password, "Password"),
];
const CONNECT_AUTH: &[(ConnectAuth, &str)] =
    &[(ConnectAuth::Silent, "Silent"), (ConnectAuth::Browser, "Browser")];
const CERTS: &[(CertPolicy, &str)] = &[
    (CertPolicy::Ask, "Ask"),
    (CertPolicy::TrustOnFirstUse, "Trust first use"),
    (CertPolicy::Ignore, "Ignore (unsafe)"),
];
const HOTKEYS: &[(HotkeyModifier, &str)] = &[
    (HotkeyModifier::RightCtrl, "Right Ctrl"),
    (HotkeyModifier::CtrlAlt, "Ctrl + Alt"),
    (HotkeyModifier::RightShift, "Right Shift"),
    (HotkeyModifier::RightCtrlRightShift, "Right Ctrl + Shift"),
    (HotkeyModifier::Disabled, "Disabled"),
    (HotkeyModifier::Unmanaged, "Unmanaged"),
];
const LOG_LEVELS: &[(LogLevel, &str)] = &[
    (LogLevel::Warn, "Warnings"),
    (LogLevel::Info, "Info"),
    (LogLevel::Debug, "Debug"),
    (LogLevel::Trace, "Trace"),
];

/// Position of `value` in `items` (0 when absent).
pub fn index_of<T: PartialEq>(items: &[(T, &str)], value: &T) -> u32 {
    items.iter().position(|(v, _)| v == value).unwrap_or(0) as u32
}

/// Build the dialog. `on_change` receives the full settings after every edit.
pub fn build(context: SettingsContext, initial: ConnectionSettings, on_change: impl Fn(ConnectionSettings) + 'static) -> adw::PreferencesDialog {
    let shared: Shared = Rc::new(RefCell::new(initial));
    let on_change: OnChange = Rc::new(on_change);
    let flushes: FlushList = Rc::new(RefCell::new(Vec::new()));
    let rows = Rows { shared, on_change, flushes: flushes.clone() };

    let dialog = adw::PreferencesDialog::new();
    dialog.set_search_enabled(true);
    // Room for explanatory subtitles next to the chosen values.
    dialog.set_content_width(760);
    dialog.set_title(&match &context.target {
        SettingsTarget::Defaults => "Default Settings".to_owned(),
        SettingsTarget::Resource { title, .. } => format!("Settings for {title}"),
    });

    dialog.add(&display_page(&rows, &context));
    dialog.add(&graphics_page(&rows));
    dialog.add(&audio_page(&rows));
    dialog.add(&devices_page(&rows));
    dialog.add(&advanced_page(&rows, &context));
    dialog.connect_closed(move |_| {
        for flush in flushes.borrow_mut().drain(..) {
            flush();
        }
    });
    dialog
}

fn commit_settings(shared: &Shared, on_change: &OnChange, edit: impl FnOnce(&mut ConnectionSettings)) {
    let snapshot = {
        let mut settings = shared.borrow_mut();
        edit(&mut settings);
        settings.clone()
    };
    on_change(snapshot);
}

/// Row builders bound to the shared settings.
struct Rows {
    shared: Shared,
    on_change: OnChange,
    flushes: FlushList,
}

impl Rows {
    fn commit(&self, edit: impl FnOnce(&mut ConnectionSettings)) {
        commit_settings(&self.shared, &self.on_change, edit);
    }

    fn handle(&self) -> Rows {
        Rows { shared: self.shared.clone(), on_change: self.on_change.clone(), flushes: self.flushes.clone() }
    }

    fn switch(
        &self,
        group: &adw::PreferencesGroup,
        title: &str,
        subtitle: &str,
        get: fn(&ConnectionSettings) -> bool,
        set: fn(&mut ConnectionSettings, bool),
    ) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).build();
        row.set_active(get(&self.shared.borrow()));
        let rows = self.handle();
        row.connect_active_notify(move |row| {
            let value = row.is_active();
            rows.commit(|s| set(s, value));
        });
        group.add(&row);
        row
    }

    /// A choice row. A drop-down suffix rather than `adw::ComboRow`: the
    /// combo row shrinks its value (and drops its arrow) next to a long
    /// subtitle, while a drop-down keeps its width and the subtitle wraps.
    fn combo<T: Copy + PartialEq + 'static>(
        &self,
        group: &adw::PreferencesGroup,
        title: &str,
        subtitle: &str,
        items: &'static [(T, &'static str)],
        get: fn(&ConnectionSettings) -> T,
        set: fn(&mut ConnectionSettings, T),
    ) -> adw::ActionRow {
        let labels: Vec<&str> = items.iter().map(|(_, label)| *label).collect();
        let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
        let dropdown = gtk::DropDown::from_strings(&labels);
        dropdown.set_valign(gtk::Align::Center);
        // A box shares out space below natural widths when a long subtitle
        // competes, which would clip the drop-down's arrow. Once styled, keep
        // it at its natural width; the subtitle wraps instead.
        dropdown.connect_map(pin_natural_width);
        dropdown.set_selected(index_of(items, &get(&self.shared.borrow())));
        dropdown.update_property(&[gtk::accessible::Property::Label(title)]);
        row.add_suffix(&dropdown);
        row.set_activatable_widget(Some(&dropdown));
        let rows = self.handle();
        dropdown.connect_selected_notify(move |dropdown| {
            pin_natural_width(dropdown);
            if let Some((value, _)) = items.get(dropdown.selected() as usize) {
                let value = *value;
                rows.commit(|s| set(s, value));
            }
        });
        group.add(&row);
        row
    }

    #[allow(clippy::too_many_arguments)]
    fn spin(
        &self,
        group: &adw::PreferencesGroup,
        title: &str,
        subtitle: &str,
        range: (f64, f64, f64),
        get: fn(&ConnectionSettings) -> u32,
        set: fn(&mut ConnectionSettings, u32),
    ) -> adw::SpinRow {
        let row = adw::SpinRow::with_range(range.0, range.1, range.2);
        row.set_title(title);
        row.set_subtitle(subtitle);
        row.set_value(f64::from(get(&self.shared.borrow())));
        let rows = self.handle();
        row.connect_value_notify(move |row| {
            let value = row.value().round().max(0.0) as u32;
            rows.commit(|s| set(s, value));
        });
        group.add(&row);
        row
    }

    fn entry(
        &self,
        group: &adw::PreferencesGroup,
        title: &str,
        get: fn(&ConnectionSettings) -> String,
        set: fn(&mut ConnectionSettings, String),
    ) -> adw::EntryRow {
        let row = adw::EntryRow::builder().title(title).build();
        row.set_text(&get(&self.shared.borrow()));
        let pending: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
        let dirty = Rc::new(Cell::new(false));
        let commit_now = {
            let pending = pending.clone();
            let dirty = dirty.clone();
            let row_weak = row.downgrade();
            let shared = self.shared.clone();
            let on_change = self.on_change.clone();
            Rc::new(move || {
                if let Some(id) = pending.borrow_mut().take() {
                    id.remove();
                }
                if dirty.replace(false)
                    && let Some(row) = row_weak.upgrade()
                {
                    let value = row.text().to_string();
                    commit_settings(&shared, &on_change, |s| set(s, value));
                }
            })
        };
        {
            let flush = commit_now.clone();
            self.flushes.borrow_mut().push(Box::new(move || flush()));
        }
        row.connect_changed(move |_| {
            dirty.set(true);
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let pending_timer = pending.clone();
            let commit_timer = commit_now.clone();
            let id = gtk::glib::timeout_add_local_once(Duration::from_millis(350), move || {
                *pending_timer.borrow_mut() = None;
                commit_timer();
            });
            *pending.borrow_mut() = Some(id);
        });
        group.add(&row);
        row
    }
}

fn bind_visibility(combo_row: &adw::ActionRow, shared: &Shared, update: impl Fn(&ConnectionSettings) + 'static) {
    update(&shared.borrow());
    if let Some(dropdown) = combo_row.activatable_widget().and_downcast::<gtk::DropDown>() {
        let shared = shared.clone();
        dropdown.connect_selected_notify(move |_| update(&shared.borrow()));
    }
}

fn pin_natural_width(dropdown: &gtk::DropDown) {
    dropdown.set_size_request(-1, -1);
    let (_, natural, _, _) = dropdown.measure(gtk::Orientation::Horizontal, -1);
    dropdown.set_size_request(natural, -1);
}

fn page(title: &str, icon: &str) -> adw::PreferencesPage {
    adw::PreferencesPage::builder().title(title).icon_name(icon).build()
}

fn group(page: &adw::PreferencesPage, title: &str, description: &str) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    if !description.is_empty() {
        group.set_description(Some(description));
    }
    page.add(&group);
    group
}

fn display_page(rows: &Rows, context: &SettingsContext) -> adw::PreferencesPage {
    let page = page("Display", "video-display-symbolic");
    if let SettingsTarget::Resource { title, .. } = &context.target {
        let note = group(
            &page,
            "",
            &format!("These settings apply only to {title}. Use “Reset to defaults” in its menu to follow the default settings again."),
        );
        note.add_css_class("scope-note");
    }

    let display = group(&page, "Display", "");
    let mode_row = rows.combo(
        &display,
        "Mode",
        "Full screen uses the monitor's native resolution. A resizable window changes the remote resolution as you resize it.",
        DISPLAY_MODES,
        |s| s.display.mode,
        |s, v| s.display.mode = v,
    );
    let monitors_row = rows.switch(
        &display,
        "Use all monitors",
        "Full screen only. FreeRDP's multi-monitor layout is known to fail at fractional scales such as 125 % or 150 %.",
        |s| s.display.multi_monitor,
        |s, v| s.display.multi_monitor = v,
    );
    let width_row = rows.spin(
        &display,
        "Window width",
        "Starting size of a window, and the remote resolution of a fixed-size window.",
        (640.0, 16384.0, 16.0),
        |s| s.display.width,
        |s, v| s.display.width = v,
    );
    let height_row =
        rows.spin(&display, "Window height", "", (480.0, 16384.0, 16.0), |s| s.display.height, |s, v| s.display.height = v);
    let stretch_row = rows.switch(
        &display,
        "Stretch to the window",
        "Fixed-size windows only: scale the remote image to fit instead of showing scroll bars. Scaling softens text.",
        |s| s.display.smart_sizing,
        |s, v| s.display.smart_sizing = v,
    );
    rows.combo(
        &display,
        "Connection bar",
        "Floating menu at the top of the remote desktop to switch back, view logs, or disconnect. Auto-hide reveals when hovering the top-center edge (Wayland overlay requires gtk4-layer-shell on SDL; built-in on X11).",
        CONNECTION_BAR,
        |s| s.display.connection_bar,
        |s, v| s.display.connection_bar = v,
    );

    // Only show the rows that mean something in the chosen mode: full screen
    // always uses the monitor's own resolution, so it has no size to set.
    bind_visibility(&mode_row, &rows.shared, move |s| {
        let mode = s.display.mode;
        monitors_row.set_visible(mode == DisplayMode::Fullscreen);
        width_row.set_visible(mode != DisplayMode::Fullscreen);
        height_row.set_visible(mode != DisplayMode::Fullscreen);
        stretch_row.set_visible(mode == DisplayMode::FixedWindow);
    });

    let local = context.local_scale.map(|p| format!(" (this monitor: {p} %)")).unwrap_or_default();
    let scaling = group(&page, "Scaling", "How large text and controls are drawn inside Windows.");
    let scale_row = rows.combo(
        &scaling,
        "Scale",
        &format!("Automatic uses this monitor's display scale{local}."),
        SCALE_MODES,
        |s| s.display.scale,
        |s, v| s.display.scale = v,
    );
    let custom_scale_row = rows.spin(
        &scaling,
        "Custom scale (%)",
        "Used when Scale is Custom. Only applies with a single monitor.",
        (100.0, 500.0, 5.0),
        |s| s.display.scale_percent,
        |s, v| s.display.scale_percent = v,
    );
    bind_visibility(&scale_row, &rows.shared, move |s| {
        custom_scale_row.set_visible(s.display.scale == ScaleMode::Custom);
    });
    page
}

fn graphics_page(rows: &Rows) -> adw::PreferencesPage {
    let page = page("Graphics", "applications-graphics-symbolic");
    let quality = group(
        &page,
        "Quality",
        "The frame rate is capped by the remote host: usually 30 fps, or 60 fps if the host allows it. No client setting raises it; these settings trade image quality against bandwidth and CPU.",
    );
    rows.combo(
        &quality,
        "Codec",
        "AVC 4:4:4 gives the sharpest text and uses the most CPU. AVC 4:2:0 uses less but makes coloured text slightly soft. RemoteFX avoids H.264; legacy bitmaps are for troubleshooting only.",
        CODECS,
        |s| s.graphics.codec,
        |s, v| s.graphics.codec = v,
    );
    rows.combo(
        &quality,
        "Network",
        "Automatic measures the link and adapts. A fixed profile only helps when detection picks badly.",
        NETWORKS,
        |s| s.graphics.network,
        |s, v| s.graphics.network = v,
    );
    rows.switch(
        &quality,
        "Video redirection",
        "Sends some video streams separately (FreeRDP's video and geometry channels). Needs host support; turn off if video stutters.",
        |s| s.graphics.video_channel,
        |s, v| s.graphics.video_channel = v,
    );

    let effects = group(&page, "Visual effects", "Turning effects off saves bandwidth on a slow link. On a fast link keep them on.");
    rows.switch(&effects, "Desktop wallpaper", "", |s| s.graphics.wallpaper, |s, v| s.graphics.wallpaper = v);
    rows.switch(&effects, "Visual styles", "Themed windows and buttons.", |s| s.graphics.themes, |s, v| s.graphics.themes = v);
    rows.switch(
        &effects,
        "Font smoothing",
        "ClearType in the remote session. Keep on for readable text.",
        |s| s.graphics.font_smoothing,
        |s, v| s.graphics.font_smoothing = v,
    );
    rows.switch(
        &effects,
        "Desktop composition",
        "Transparency and other composited effects.",
        |s| s.graphics.desktop_composition,
        |s, v| s.graphics.desktop_composition = v,
    );
    rows.switch(
        &effects,
        "Show window contents while dragging",
        "",
        |s| s.graphics.window_drag,
        |s, v| s.graphics.window_drag = v,
    );
    rows.switch(&effects, "Menu and window animations", "", |s| s.graphics.menu_animations, |s, v| s.graphics.menu_animations = v);
    page
}

fn audio_page(rows: &Rows) -> adw::PreferencesPage {
    let page = page("Audio", "audio-speakers-symbolic");
    let sound = group(&page, "Sound", "");
    rows.combo(&sound, "Play sound", "", PLAYBACK, |s| s.audio.playback, |s, v| s.audio.playback = v);
    rows.combo(
        &sound,
        "Sound system",
        "PipeWire serves the PulseAudio interface, so PulseAudio is right on most systems. Choose ALSA only without a sound server.",
        AUDIO_BACKENDS,
        |s| s.audio.backend,
        |s, v| s.audio.backend = v,
    );
    rows.combo(
        &sound,
        "Quality",
        "High asks for uncompressed sound where the host allows it. Dynamic lets the host adapt to the link.",
        AUDIO_QUALITY,
        |s| s.audio.quality,
        |s, v| s.audio.quality = v,
    );
    rows.spin(
        &sound,
        "Playback buffer (ms)",
        "0 keeps FreeRDP's default. Raise it if sound crackles; lower values reduce delay.",
        (0.0, 2000.0, 10.0),
        |s| s.audio.latency_ms,
        |s, v| s.audio.latency_ms = v,
    );
    let mic = group(&page, "Microphone", "");
    rows.switch(
        &mic,
        "Use the microphone",
        "Lets the remote computer record from this computer's default input.",
        |s| s.audio.microphone,
        |s, v| s.audio.microphone = v,
    );
    page
}

fn devices_page(rows: &Rows) -> adw::PreferencesPage {
    let page = page("Devices", "drive-harddisk-symbolic");
    let sharing = group(&page, "Sharing", "");
    rows.switch(&sharing, "Clipboard", "Copy and paste text and images between both computers.", |s| s.devices.clipboard, |s, v| s.devices.clipboard = v);
    let drives_row = rows.combo(
        &sharing,
        "Share files",
        "The shared folder appears in the remote File Explorer under This PC.",
        DRIVES,
        |s| s.devices.drives,
        |s, v| s.devices.drives = v,
    );
    let folder_row = rows.entry(&sharing, "Folder to share (for “A chosen folder”)", |s| s.devices.shared_folder.clone(), |s, v| {
        s.devices.shared_folder = v
    });
    bind_visibility(&drives_row, &rows.shared, move |s| {
        folder_row.set_visible(s.devices.drives == DriveSharing::Folder);
    });
    let hardware = group(&page, "Hardware", "These need support on the host and may be blocked by its policy.");
    rows.switch(&hardware, "Camera", "Redirects local cameras.", |s| s.devices.camera, |s, v| s.devices.camera = v);
    rows.switch(&hardware, "Printers", "Makes local printers available remotely.", |s| s.devices.printers, |s, v| s.devices.printers = v);
    rows.switch(&hardware, "Smart cards", "", |s| s.devices.smartcards, |s, v| s.devices.smartcards = v);
    page
}

fn advanced_page(rows: &Rows, context: &SettingsContext) -> adw::PreferencesPage {
    let page = page("Advanced", "preferences-other-symbolic");

    let client = group(&page, "FreeRDP client", "");
    rows.combo(
        &client,
        "Client",
        "SDL runs natively on Wayland and is actively developed. X11 runs through XWayland.",
        CLIENTS,
        |s| s.advanced.client,
        |s, v| s.advanced.client = v,
    );
    rows.switch(
        &client,
        "Open apps with the X11 client",
        "The X11 client integrates RemoteApp windows more completely.",
        |s| s.advanced.remote_apps_use_x11,
        |s, v| s.advanced.remote_apps_use_x11 = v,
    );
    rows.combo(
        &client,
        "Video driver",
        "SDL client only. Automatic uses Wayland on a Wayland session.",
        VIDEO_BACKENDS,
        |s| s.advanced.video_backend,
        |s, v| s.advanced.video_backend = v,
    );

    let sign_in = group(&page, "Sign-in", "");
    rows.combo(
        &sign_in,
        "Session host sign-in",
        "Automatic follows the connection file. Single sign-on uses your Microsoft account; Password asks for the remote computer's password.",
        HOST_AUTH,
        |s| s.advanced.host_auth,
        |s, v| s.advanced.host_auth = v,
    );
    rows.combo(
        &sign_in,
        "Connection sign-in",
        "Silent issues FreeRDP's tokens from your saved sign-in, without a browser window. Browser opens a sign-in page for every connection.",
        CONNECT_AUTH,
        |s| s.advanced.connect_auth,
        |s, v| s.advanced.connect_auth = v,
    );
    rows.combo(
        &sign_in,
        "Unverified certificates",
        "Ask shows FreeRDP's prompt (the X11 client trusts on first use instead). Ignore is unsafe: it accepts any certificate, including an attacker's.",
        CERTS,
        |s| s.advanced.certificates,
        |s, v| s.advanced.certificates = v,
    );

    let keyboard = group(&page, "Keyboard", "");
    rows.combo(
        &keyboard,
        "Shortcut modifier",
        "Modifier for SDL client shortcuts (+ Enter full screen — press twice when starting full screen on FreeRDP 3.33, + M minimize, + G toggle keyboard grab, + D disconnect). Right Ctrl leaves Ctrl + Alt free for Windows and avoids desktop shortcut conflicts. Written to ~/.config/freerdp/sdl-freerdp.json.",
        HOTKEYS,
        |s| s.advanced.hotkeys,
        |s, v| s.advanced.hotkeys = v,
    );
    rows.switch(
        &keyboard,
        "Send all keys to the remote",
        "Includes Super and Alt+Tab while focused. On KDE/GNOME Wayland, allow shortcut inhibition if prompted; if your desktop still intercepts Ctrl + Alt, try the X11 video driver or X11 client.",
        |s| s.advanced.grab_keyboard,
        |s, v| s.advanced.grab_keyboard = v,
    );

    let connection = group(&page, "Connection", "");
    rows.switch(
        &connection,
        "Reconnect automatically",
        "Retries up to 20 times when the network drops.",
        |s| s.advanced.auto_reconnect,
        |s, v| s.advanced.auto_reconnect = v,
    );

    let troubleshooting = group(&page, "Troubleshooting", "");
    rows.combo(
        &troubleshooting,
        "Log detail",
        "Debug and Trace logs can contain sign-in tokens. Do not share them without checking.",
        LOG_LEVELS,
        |s| s.advanced.log_level,
        |s, v| s.advanced.log_level = v,
    );
    let extra = rows.entry(&troubleshooting, "Extra FreeRDP arguments", |s| s.advanced.extra_args.clone(), |s, v| {
        s.advanced.extra_args = v
    });
    extra.set_tooltip_text(Some("Appended last, so they override the settings above. Example: /kbd:layout:0x409"));

    if let Some((sdl, x11)) = &context.client_paths {
        let programs = group(
            &page,
            "FreeRDP programs",
            "Leave empty to search PATH. FreeRDP 3.32 or later is needed for single sign-on.",
        );
        let paths = Rc::new(RefCell::new((sdl.clone(), x11.clone())));
        for (index, title, kind) in [(0, "SDL client path", ClientKind::Sdl), (1, "X11 client path", ClientKind::X11)] {
            let row = adw::EntryRow::builder().title(title).show_apply_button(true).build();
            row.set_text(if index == 0 { sdl } else { x11 });
            let status = client_status(context.clients.get(kind));
            let label = gtk::Label::builder().label(&status).css_classes(["dim-label", "caption"]).build();
            row.add_suffix(&label);
            let paths = paths.clone();
            let on_paths = context.on_paths.clone();
            let label_weak = label.downgrade();
            let detect_gen = Rc::new(Cell::new(0u64));
            row.connect_apply(move |row| {
                let text = row.text().trim().to_owned();
                let (sdl, x11) = {
                    let mut current = paths.borrow_mut();
                    if index == 0 {
                        current.0 = text.clone();
                    } else {
                        current.1 = text.clone();
                    }
                    current.clone()
                };
                on_paths(sdl, x11);
                let generation = detect_gen.get().wrapping_add(1);
                detect_gen.set(generation);
                if let Some(label) = label_weak.upgrade() {
                    label.set_label("checking…");
                }
                let label_weak = label_weak.clone();
                let detect_gen = detect_gen.clone();
                relm4::spawn_local(async move {
                    let detected = gtk::gio::spawn_blocking(move || freerdp::detect(kind, &text)).await;
                    if let Ok(client) = detected
                        && detect_gen.get() == generation
                        && let Some(label) = label_weak.upgrade()
                    {
                        label.set_label(&client_status(client.as_ref()));
                    }
                });
            });
            programs.add(&row);
        }
    }
    page
}

fn client_status(client: Option<&freerdp::Client>) -> String {
    match client {
        Some(client) => format!("{} {}", client.path.display(), client.version_string()),
        None => "not found".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_enum_value_has_a_label() {
        fn check<T: Copy + PartialEq + std::fmt::Debug>(items: &[(T, &str)], values: &[T]) {
            for value in values {
                let index = index_of(items, value) as usize;
                assert_eq!(&items[index].0, value, "{value:?} missing");
            }
        }
        check(DISPLAY_MODES, &[DisplayMode::Fullscreen, DisplayMode::DynamicWindow, DisplayMode::FixedWindow]);
        check(CODECS, &[GraphicsCodec::Avc444, GraphicsCodec::Avc420, GraphicsCodec::Progressive, GraphicsCodec::Legacy]);
        check(
            NETWORKS,
            &[
                NetworkProfile::Auto,
                NetworkProfile::Lan,
                NetworkProfile::BroadbandHigh,
                NetworkProfile::Broadband,
                NetworkProfile::BroadbandLow,
                NetworkProfile::Wan,
                NetworkProfile::Modem,
            ],
        );
        check(PLAYBACK, &[AudioPlayback::Local, AudioPlayback::Remote, AudioPlayback::Off]);
        check(AUDIO_QUALITY, &[AudioQuality::High, AudioQuality::Medium, AudioQuality::Dynamic]);
        check(DRIVES, &[DriveSharing::Off, DriveSharing::Home, DriveSharing::Folder]);
        check(HOST_AUTH, &[HostAuth::Auto, HostAuth::EntraSso, HostAuth::Password]);
        check(CERTS, &[CertPolicy::Ask, CertPolicy::TrustOnFirstUse, CertPolicy::Ignore]);
        check(
            HOTKEYS,
            &[
                HotkeyModifier::RightCtrl,
                HotkeyModifier::CtrlAlt,
                HotkeyModifier::RightShift,
                HotkeyModifier::RightCtrlRightShift,
                HotkeyModifier::Disabled,
                HotkeyModifier::Unmanaged,
            ],
        );
        check(CONNECTION_BAR, &[ConnectionBar::AutoHide, ConnectionBar::Pinned, ConnectionBar::Off]);
        check(LOG_LEVELS, &[LogLevel::Warn, LogLevel::Info, LogLevel::Debug, LogLevel::Trace]);
        check(VIDEO_BACKENDS, &[VideoBackend::Auto, VideoBackend::Wayland, VideoBackend::X11]);
        check(CLIENTS, &[ClientKind::Sdl, ClientKind::X11]);
        check(CONNECT_AUTH, &[ConnectAuth::Silent, ConnectAuth::Browser]);
        check(SCALE_MODES, &[ScaleMode::Auto, ScaleMode::Custom]);
        check(AUDIO_BACKENDS, &[AudioBackend::Pulse, AudioBackend::Alsa]);
    }
}
