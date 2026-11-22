//! Widget tree of the main window and how it follows the model.

use adw::prelude::*;
use relm4::ComponentSender;
use relm4::gtk::gio;

use super::app::{App, AppMsg, Page, SignInState};
use super::logic::{FeedView, client_banner, feed_view, warnings_text};
use crate::consts;

pub const CSS: &str = r#"
.resource-card { padding: 0; }
.resource-card .card-main { padding: 20px 12px 14px 12px; border-radius: 12px; }
.resource-card .card-menu { min-width: 28px; min-height: 28px; }
.badge { border-radius: 999px; padding: 2px 10px; font-weight: bold; font-size: smaller;
         background-color: alpha(@success_color, 0.15); }
.small-pill { padding: 2px 12px; min-height: 26px; }
.feed-warning { background-color: alpha(@warning_color, 0.14); border-radius: 12px; padding: 10px 14px; }
.workspace-title { margin-left: 4px; }
.signin-error { margin-top: 6px; }

"#;

pub struct AppWidgets {
    pub window_title: adw::WindowTitle,
    pub refresh_stack: gtk::Stack,
    pub search_button: gtk::ToggleButton,
    pub search_bar: gtk::SearchBar,
    pub banner: adw::Banner,
    pub pages: gtk::Stack,
    pub startup_error: adw::StatusPage,
    pub signin_stack: gtk::Stack,
    pub signin_error: gtk::Label,
    pub manual_entry: gtk::Entry,
    pub manual_submit: gtk::Button,
    pub manual_spinner: adw::Spinner,
    pub warnings: gtk::Box,
    pub warnings_label: gtk::Label,
    pub body: gtk::Stack,
    pub failed_page: adw::StatusPage,
    pub nomatch_page: adw::StatusPage,
    pub actions: gio::SimpleActionGroup,
}

/// Name of the icon to use for the app: ours when the icon theme has it.
pub fn app_icon_name() -> &'static str {
    let has_ours = gtk::gdk::Display::default()
        .map(|display| gtk::IconTheme::for_display(&display).has_icon(consts::APP_ID))
        .unwrap_or(false);
    if has_ours { consts::APP_ID } else { "computer-symbolic" }
}

/// Make the icons in the source tree available when running uninstalled.
pub fn add_source_icons() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons");
    if let Some(display) = gtk::gdk::Display::default()
        && std::path::Path::new(dir).is_dir()
    {
        gtk::IconTheme::for_display(&display).add_search_path(dir);
    }
}

fn status_page(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder().icon_name(icon).title(title).description(description).vexpand(true).build()
}

fn spinner_page(text: &str) -> gtk::Box {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .vexpand(true)
        .build();
    let spinner = adw::Spinner::new();
    spinner.set_size_request(48, 48);
    page.append(&spinner);
    page.append(&gtk::Label::builder().label(text).css_classes(["dim-label"]).build());
    page
}

fn pill(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::builder().label(label).halign(gtk::Align::Center).css_classes(["pill"]).build();
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn send_on_click(button: &gtk::Button, sender: &ComponentSender<App>, make: fn() -> AppMsg) {
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(make()));
}

fn primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let first = gio::Menu::new();
    first.append(Some("Default Settings…"), Some("win.defaults"));
    first.append(Some("Open .rdp File…"), Some("win.open-rdp"));
    first.append(Some("Open Logs Folder"), Some("win.logs"));
    menu.append_section(None, &first);
    let second = gio::Menu::new();
    second.append(Some(&format!("About {}", consts::APP_NAME)), Some("win.about"));
    second.append(Some("Sign Out"), Some("win.sign-out"));
    menu.append_section(None, &second);
    menu
}

fn sign_in_page(sender: &ComponentSender<App>, w: &mut Partial) -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name(app_icon_name())
        .title(format!("Welcome to {}", consts::APP_NAME))
        .description(
            "Sign in with your work or school account to see the Azure Virtual Desktop and Windows 365 desktops and apps assigned to you.",
        )
        .vexpand(true)
        .build();

    // Not started.
    let idle = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    let sign_in = pill("Sign In", true);
    send_on_click(&sign_in, sender, || AppMsg::SignIn);
    idle.append(&sign_in);
    idle.append(
        &gtk::Label::builder()
            .label(format!("Signing in opens your web browser. At the end it asks to open {}: allow it.", consts::APP_NAME))
            .css_classes(["dim-label", "caption"])
            .build(),
    );

    // Waiting for the browser.
    let waiting = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).halign(gtk::Align::Center).build();
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    waiting.append(&spinner);
    waiting.append(&gtk::Label::builder().label("Finish signing in in your browser").css_classes(["title-4"]).build());
    waiting.append(
        &gtk::Label::builder()
            .label(format!(
                "When you have signed in, the browser asks to open {} (“Open link” or “Allow”). Allow it, and this page continues by itself.",
                consts::APP_NAME
            ))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );
    let buttons = gtk::Box::builder().spacing(12).halign(gtk::Align::Center).build();
    let cancel = pill("Cancel", false);
    send_on_click(&cancel, sender, || AppMsg::CancelSignIn);
    let copy = pill("Copy Link", false);
    copy.set_tooltip_text(Some("Copy the sign-in address, for when the browser did not open"));
    send_on_click(&copy, sender, || AppMsg::CopySignInLink);
    buttons.append(&cancel);
    buttons.append(&copy);
    waiting.append(&buttons);
    let paste = gtk::Button::builder().label("Paste the address instead").css_classes(["flat"]).halign(gtk::Align::Center).build();
    paste.set_tooltip_text(Some("For browsers that cannot return to this app: sign in, then paste the final address"));
    send_on_click(&paste, sender, || AppMsg::UseManualSignIn);
    waiting.append(&paste);

    // Paste-the-address fallback.
    let manual = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    let clamp = adw::Clamp::builder().maximum_size(560).child(&manual).build();
    manual.append(
        &gtk::Label::builder()
            .label("Sign in in the browser window that just opened. It ends on a blank page: copy the whole address from the address bar and paste it here.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build(),
    );
    let entry = gtk::Entry::builder()
        .placeholder_text("https://login.microsoftonline.com/common/oauth2/nativeclient?code=…")
        .hexpand(true)
        .build();
    {
        let sender = sender.clone();
        entry.connect_activate(move |entry| sender.input(AppMsg::SubmitPasted(entry.text().to_string())));
    }
    manual.append(&entry);
    let row = gtk::Box::builder().spacing(12).halign(gtk::Align::Center).build();
    let manual_cancel = pill("Cancel", false);
    send_on_click(&manual_cancel, sender, || AppMsg::CancelSignIn);
    let manual_copy = pill("Copy Link", false);
    send_on_click(&manual_copy, sender, || AppMsg::CopySignInLink);
    let submit = pill("Continue", true);
    {
        let sender = sender.clone();
        let entry = entry.clone();
        submit.connect_clicked(move |_| sender.input(AppMsg::SubmitPasted(entry.text().to_string())));
    }
    let manual_spinner = adw::Spinner::new();
    row.append(&manual_cancel);
    row.append(&manual_copy);
    row.append(&submit);
    row.append(&manual_spinner);
    manual.append(&row);

    let stack = gtk::Stack::builder().vhomogeneous(false).transition_type(gtk::StackTransitionType::Crossfade).build();
    stack.add_named(&idle, Some("idle"));
    stack.add_named(&waiting, Some("waiting"));
    stack.add_named(&clamp, Some("manual"));

    let error = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .selectable(true)
        .max_width_chars(70)
        .css_classes(["error", "signin-error"])
        .build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    content.append(&stack);
    content.append(&error);
    page.set_child(Some(&content));

    w.signin_stack = Some(stack);
    w.signin_error = Some(error);
    w.manual_entry = Some(entry);
    w.manual_submit = Some(submit);
    w.manual_spinner = Some(manual_spinner);
    page
}

#[derive(Default)]
struct Partial {
    signin_stack: Option<gtk::Stack>,
    signin_error: Option<gtk::Label>,
    manual_entry: Option<gtk::Entry>,
    manual_submit: Option<gtk::Button>,
    manual_spinner: Option<adw::Spinner>,
}

/// Build the window contents. `sections` is where the workspace sections go.
pub fn build(
    window: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
    sections: &gtk::Box,
    sender: &ComponentSender<App>,
) -> AppWidgets {
    let mut partial = Partial::default();

    // Header bar.
    let window_title = adw::WindowTitle::new(consts::APP_NAME, "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    let refresh_button = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh (Ctrl+R)").build();
    refresh_button.set_action_name(Some("win.refresh"));
    let refresh_spinner = adw::Spinner::new();
    refresh_spinner.set_tooltip_text(Some("Refreshing…"));
    let refresh_stack = gtk::Stack::new();
    refresh_stack.add_named(&refresh_button, Some("button"));
    refresh_stack.add_named(&refresh_spinner, Some("spinner"));
    header.pack_start(&refresh_stack);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&primary_menu())
        .primary(true)
        .tooltip_text("Main Menu")
        .build();
    header.pack_end(&menu_button);
    let search_button = gtk::ToggleButton::builder().icon_name("system-search-symbolic").tooltip_text("Search (Ctrl+F)").build();
    header.pack_end(&search_button);

    // Search.
    let search_entry = gtk::SearchEntry::builder().placeholder_text("Search desktops and apps").hexpand(true).build();
    let search_clamp = adw::Clamp::builder().maximum_size(480).child(&search_entry).build();
    let search_bar = gtk::SearchBar::builder().child(&search_clamp).build();
    search_bar.connect_entry(&search_entry);
    search_bar.set_key_capture_widget(Some(window));
    search_button.bind_property("active", &search_bar, "search-mode-enabled").bidirectional().build();
    {
        let sender = sender.clone();
        search_entry.connect_search_changed(move |entry| sender.input(AppMsg::SearchChanged(entry.text().to_string())));
    }

    let banner = adw::Banner::new("");
    banner.set_revealed(false);

    // Pages.
    let pages = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    pages.add_named(&spinner_page("Starting…"), Some("loading"));
    let startup_error = status_page("dialog-error-symbolic", &format!("{} could not start", consts::APP_NAME), "");
    pages.add_named(&startup_error, Some("error"));
    pages.add_named(&sign_in_page(sender, &mut partial), Some("signin"));

    // Main page: warnings, then the grid or a state page.
    let warnings_label = gtk::Label::builder().wrap(true).xalign(0.0).hexpand(true).selectable(true).build();
    let warnings = gtk::Box::builder().spacing(12).css_classes(["feed-warning"]).margin_start(24).margin_end(24).margin_top(12).build();
    warnings.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    warnings.append(&warnings_label);

    sections.set_orientation(gtk::Orientation::Vertical);
    sections.set_spacing(28);
    sections.set_margin_top(18);
    sections.set_margin_bottom(24);
    sections.set_margin_start(24);
    sections.set_margin_end(24);
    let grid = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(sections).vexpand(true).build();

    let failed_page = status_page("network-offline-symbolic", "Your desktops could not be loaded", "");
    let retry = pill("Retry", true);
    retry.set_action_name(Some("win.refresh"));
    failed_page.set_child(Some(&retry));
    let empty_page = status_page(
        "computer-symbolic",
        "No desktops or apps",
        "No Azure Virtual Desktop or Windows 365 resources are assigned to this account. Ask your administrator, then refresh.",
    );
    let nomatch_page = status_page("system-search-symbolic", "No results", "");

    let body = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    body.add_named(&grid, Some("grid"));
    body.add_named(&spinner_page("Loading your desktops and apps…"), Some("loading"));
    body.add_named(&failed_page, Some("failed"));
    body.add_named(&empty_page, Some("empty"));
    body.add_named(&nomatch_page, Some("nomatch"));

    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.append(&warnings);
    main.append(&body);
    pages.add_named(&main, Some("main"));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&search_bar);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&pages));
    toast_overlay.set_child(Some(&toolbar));
    window.set_content(Some(toast_overlay));

    let actions = window_actions(window, sender);
    let app = relm4::main_application();
    app.set_accels_for_action("win.refresh", &["<primary>r", "F5"]);
    app.set_accels_for_action("win.search", &["<primary>f"]);
    app.set_accels_for_action("win.defaults", &["<primary>comma"]);
    {
        let search_button = search_button.clone();
        let action = gio::SimpleAction::new("search", None);
        action.connect_activate(move |_, _| search_button.set_active(!search_button.is_active()));
        actions.add_action(&action);
    }

    AppWidgets {
        window_title,
        refresh_stack,
        search_button,
        search_bar,
        banner,
        pages,
        startup_error,
        signin_stack: partial.signin_stack.unwrap(),
        signin_error: partial.signin_error.unwrap(),
        manual_entry: partial.manual_entry.unwrap(),
        manual_submit: partial.manual_submit.unwrap(),
        manual_spinner: partial.manual_spinner.unwrap(),
        warnings,
        warnings_label,
        body,
        failed_page,
        nomatch_page,
        actions,
    }
}

type WindowAction = (&'static str, fn() -> AppMsg);

fn window_actions(window: &adw::ApplicationWindow, sender: &ComponentSender<App>) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    let entries: [WindowAction; 6] = [
        ("refresh", || AppMsg::Refresh),
        ("defaults", || AppMsg::ShowDefaults),
        ("open-rdp", || AppMsg::OpenRdpFile),
        ("logs", || AppMsg::OpenLogsFolder),
        ("about", || AppMsg::ShowAbout),
        ("sign-out", || AppMsg::SignOut),
    ];
    for (name, make) in entries {
        let action = gio::SimpleAction::new(name, None);
        let sender = sender.clone();
        action.connect_activate(move |_, _| sender.input(make()));
        group.add_action(&action);
    }
    window.insert_action_group("win", Some(&group));
    group
}

fn set_enabled(group: &gio::SimpleActionGroup, name: &str, enabled: bool) {
    if let Some(action) = group.lookup_action(name).and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
        action.set_enabled(enabled);
    }
}

impl AppWidgets {
    /// Bring the widgets in line with the model.
    pub fn sync(&self, model: &App) {
        let page = match model.page {
            Page::Loading => "loading",
            Page::StartupFailed => "error",
            Page::SignIn => "signin",
            Page::Main => "main",
        };
        self.pages.set_visible_child_name(page);
        if let Some(error) = &model.startup_error {
            self.startup_error.set_description(Some(error));
        }

        let main = model.page == Page::Main;
        let ready = model.service.is_some();
        self.window_title.set_subtitle(&match (&model.account, model.demo) {
            (Some(account), true) => format!("{} (demo)", account.display_name()),
            (Some(account), false) => account.display_name().to_owned(),
            (None, true) => "Demo mode".to_owned(),
            (None, false) => String::new(),
        });
        self.refresh_stack.set_visible(main);
        self.refresh_stack.set_visible_child_name(if model.feed_loading { "spinner" } else { "button" });
        self.search_button.set_visible(main);
        if !main && self.search_button.is_active() {
            self.search_button.set_active(false);
        }
        self.search_bar.set_key_capture_widget(if main { Some(&model.window) } else { None::<&adw::ApplicationWindow> });
        set_enabled(&self.actions, "refresh", main && !model.feed_loading);
        set_enabled(&self.actions, "search", main);
        set_enabled(&self.actions, "sign-out", model.account.is_some());
        set_enabled(&self.actions, "open-rdp", main && ready && model.account.is_some());
        set_enabled(&self.actions, "defaults", ready);
        set_enabled(&self.actions, "logs", ready);

        match ready.then(|| client_banner(&model.clients, model.config.defaults.advanced.client)).flatten() {
            Some(text) => {
                self.banner.set_title(&text);
                self.banner.set_revealed(true);
            }
            None => self.banner.set_revealed(false),
        }

        // Sign-in page.
        let state = match model.sign_in {
            SignInState::Idle => "idle",
            SignInState::Waiting => "waiting",
            SignInState::Manual | SignInState::ManualSubmitting => "manual",
        };
        self.signin_stack.set_visible_child_name(state);
        let submitting = model.sign_in == SignInState::ManualSubmitting;
        self.manual_spinner.set_visible(submitting);
        self.manual_submit.set_sensitive(!submitting);
        self.manual_entry.set_sensitive(!submitting);
        self.signin_error.set_label(model.sign_in_error.as_deref().unwrap_or(""));
        self.signin_error.set_visible(model.sign_in_error.is_some());

        // Main page.
        match model.feed.as_ref().and_then(|f| warnings_text(&f.warnings)) {
            Some(text) => {
                self.warnings_label.set_label(&text);
                self.warnings.set_visible(true);
            }
            None => self.warnings.set_visible(false),
        }
        let view = feed_view(model.feed.as_ref(), model.feed_loading || model.feed_error.is_none(), &model.query);
        self.body.set_visible_child_name(match view {
            FeedView::Loading => "loading",
            FeedView::Failed => "failed",
            FeedView::Empty => "empty",
            FeedView::NoMatches => "nomatch",
            FeedView::Grid => "grid",
        });
        if let Some(error) = &model.feed_error {
            self.failed_page.set_description(Some(error));
        }
        self.nomatch_page.set_description(Some(&format!("Nothing matches “{}”.", model.query.trim())));
    }
}
