//! The main component: start-up, sign-in, the feed grid and sessions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use adw::prelude::*;
use relm4::abstractions::Toaster;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;

use super::card::{Card, CardInit, CardMsg, CardOutput};
use super::demo::Demo;
use super::dialogs;
use super::logic::{self, CardState, ExitInfo, error_text, sign_in_error_text};
use super::logview::{LogStore, LogViewer, LogViewerMsg};
use super::service::{ManualLogin, Service, SessionHandle, rdp_file_title};
use super::settings::{self, SettingsContext, SettingsTarget};
use super::view::{self, AppWidgets};
use crate::auth::Account;
use crate::backend::{Backend, BackendError, Clients, OpenUrl, StatusSink};
use crate::config::{Config, ConnectionSettings};
use crate::feed::{Feed, Resource};
use crate::paths;
use crate::rdp::freerdp;
use crate::rdp::session::{EventSink, SessionEvent};

/// FreeRDP processes (real or simulated) that have not reported their exit.
static LIVE_SESSIONS: AtomicIsize = AtomicIsize::new(0);

/// After the window closed: give stopped sessions time to end cleanly, so
/// no FreeRDP process outlives the app.
pub fn wait_for_sessions(budget: Duration) {
    let start = Instant::now();
    while LIVE_SESSIONS.load(Ordering::SeqCst) > 0 && start.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn exit_line(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("--- FreeRDP ended with exit code {code} ---"),
        None => "--- FreeRDP was stopped ---".to_owned(),
    }
}

pub struct AppInit {
    pub demo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Loading,
    StartupFailed,
    SignIn,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInState {
    Idle,
    Waiting,
    Manual,
    ManualSubmitting,
}

/// What a session connects to.
#[derive(Debug, Clone)]
enum Target {
    Resource(Resource),
    File { title: String, text: String },
}

struct ActiveSession {
    generation: u64,
    title: String,
    target: Target,
    handle: Option<SessionHandle>,
    connected: bool,
    stopped: bool,
    tried_saved_password: bool,
    /// The password in use is (now) in the keyring; forget it if rejected.
    password_stored: bool,
    explanation: Option<&'static str>,
}

struct Section {
    root: gtk::Box,
    cards: FactoryVecDeque<Card>,
    /// key, title, workspace of each card, in order.
    entries: Vec<(String, String, String)>,
}

pub struct App {
    pub(super) window: adw::ApplicationWindow,
    toaster: Toaster,
    pub(super) demo: bool,
    pub(super) config: Config,
    config_path: PathBuf,
    pub(super) service: Option<Service>,
    pub(super) clients: Clients,
    pub(super) page: Page,
    pub(super) startup_error: Option<String>,
    pub(super) sign_in: SignInState,
    pub(super) sign_in_error: Option<String>,
    sign_in_task: Option<tokio::task::AbortHandle>,
    manual: Option<Arc<ManualLogin>>,
    last_url: Option<String>,
    pub(super) account: Option<Account>,
    pub(super) feed: Option<Feed>,
    pub(super) feed_loading: bool,
    pub(super) feed_error: Option<String>,
    pub(super) query: String,
    sections: Vec<Section>,
    sections_box: gtk::Box,
    icons: HashMap<String, PathBuf>,
    sessions: HashMap<String, ActiveSession>,
    failures: HashMap<String, String>,
    titles: HashMap<String, String>,
    logs: LogStore,
    log_viewer: Controller<LogViewer>,
    next_generation: u64,
}

#[derive(Debug)]
pub enum AppMsg {
    OpenUrl(String),
    Status(String),
    SignIn,
    CancelSignIn,
    UseManualSignIn,
    SubmitPasted(String),
    CopySignInLink,
    Refresh,
    SearchChanged(String),
    Card(CardOutput),
    ShowDefaults,
    SettingsChanged(SettingsTarget, ConnectionSettings),
    ClientPathsChanged(String, String),
    OpenRdpFile,
    RdpFileChosen(PathBuf),
    OpenLogsFolder,
    ShowAbout,
    SignOut,
    PasswordGiven { key: String, password: String, remember: bool },
    PasswordCancelled(String),
    Closing,
}

#[derive(Debug)]
pub enum ConnectFailure {
    PasswordRequired,
    Other(String),
}

#[derive(Debug)]
pub enum Cmd {
    Started(Result<Service, String>),
    Restored { account: Account, ok: bool, cached: Option<Feed> },
    SignedIn(Result<Account, String>),
    FeedLoaded(Result<Feed, (String, bool)>),
    Icon { key: String, path: PathBuf },
    Launched { key: String, generation: u64, result: Result<SessionHandle, ConnectFailure> },
    SavedPassword { key: String, generation: u64, password: Option<String> },
    Session { key: String, generation: u64, event: SessionEvent },
    ClientsDetected(Clients),
    Toast(String),
    Nothing,
}

impl App {
    fn toast(&self, text: &str) {
        self.toaster.add_toast(adw::Toast::builder().title(gtk::glib::markup_escape_text(text)).timeout(5).build());
    }

    fn save_config(&self) {
        if let Err(err) = self.config.save_to(&self.config_path) {
            self.toast(&format!("The settings could not be saved: {err}."));
        }
    }

    fn local_scale(&self) -> Option<u32> {
        let surface = self.window.surface()?;
        let monitor = WidgetExt::display(&self.window).monitor_at_surface(&surface)?;
        logic::scale_percent(monitor.scale())
    }

    fn card_state(&self, key: &str) -> CardState {
        match self.sessions.get(key) {
            Some(session) if session.stopped => CardState::Idle,
            Some(session) if session.connected => CardState::Connected,
            Some(_) => CardState::Connecting,
            None => self.failures.get(key).map(|r| CardState::Failed(r.clone())).unwrap_or_default(),
        }
    }

    fn send_card(&self, key: &str, msg: impl Fn() -> CardMsg) {
        for section in &self.sections {
            if let Some(index) = section.entries.iter().position(|(k, _, _)| k == key) {
                section.cards.send(index, msg());
            }
        }
    }

    fn refresh_card(&self, key: &str) {
        let state = self.card_state(key);
        self.send_card(key, || CardMsg::SetState(state.clone()));
    }

    fn title_of(&self, key: &str) -> String {
        self.titles.get(key).cloned().unwrap_or_else(|| "this desktop".to_owned())
    }

    // --- feed -------------------------------------------------------------------

    fn show_feed(&mut self, feed: Feed, sender: &ComponentSender<Self>) {
        if self.feed.as_ref() == Some(&feed) {
            return;
        }
        for resource in feed.resources() {
            self.titles.insert(resource.key(), resource.title.clone());
        }
        self.feed = Some(feed);
        self.rebuild_sections(sender);
        self.fetch_icons(sender);
    }

    fn clear_feed(&mut self) {
        self.feed = None;
        self.feed_error = None;
        self.feed_loading = false;
        while let Some(child) = self.sections_box.first_child() {
            self.sections_box.remove(&child);
        }
        self.sections.clear();
    }

    fn rebuild_sections(&mut self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.sections_box.first_child() {
            self.sections_box.remove(&child);
        }
        self.sections.clear();
        let Some(feed) = self.feed.clone() else { return };
        for workspace in feed.workspaces.iter().filter(|w| !w.resources.is_empty()) {
            let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
            let label = gtk::Label::builder()
                .label(&workspace.name)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["title-4", "workspace-title"])
                .build();
            let flow = gtk::FlowBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .column_spacing(12)
                .row_spacing(12)
                .min_children_per_line(1)
                .max_children_per_line(24)
                .homogeneous(false)
                .halign(gtk::Align::Fill)
                .valign(gtk::Align::Start)
                .build();
            root.append(&label);
            root.append(&flow);
            self.sections_box.append(&root);

            let mut cards = FactoryVecDeque::builder().launch(flow).forward(sender.input_sender(), AppMsg::Card);
            let mut entries = Vec::new();
            {
                let mut guard = cards.guard();
                for resource in &workspace.resources {
                    let key = resource.key();
                    guard.push_back(CardInit {
                        resource: resource.clone(),
                        state: self.card_state(&key),
                        has_override: self.config.has_override(&key),
                        icon: self.icons.get(&key).cloned(),
                        query: self.query.clone(),
                    });
                    entries.push((key, resource.title.clone(), resource.workspace.clone()));
                }
            }
            let section = Section { root, cards, entries };
            self.apply_filter_to(&section);
            self.sections.push(section);
        }
    }

    fn apply_filter_to(&self, section: &Section) {
        let any = section.entries.iter().any(|(_, title, ws)| logic::matches_search(&self.query, title, ws));
        section.root.set_visible(any);
    }

    fn fetch_icons(&self, sender: &ComponentSender<Self>) {
        let (Some(service), Some(feed)) = (self.service.clone(), self.feed.as_ref()) else { return };
        let wanted: Vec<Resource> = feed
            .resources()
            .filter(|r| r.icon_url.is_some() && !self.icons.contains_key(&r.key()))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return;
        }
        let out = sender.command_sender().clone();
        sender.oneshot_command(async move {
            for resource in wanted {
                if let Some(path) = service.icon(&resource).await {
                    let _ = out.send(Cmd::Icon { key: resource.key(), path });
                }
            }
            Cmd::Nothing
        });
    }

    fn refresh(&mut self, sender: &ComponentSender<Self>) {
        let Some(service) = self.service.clone() else { return };
        if self.account.is_none() || self.feed_loading {
            return;
        }
        self.feed_loading = true;
        sender.oneshot_command(async move {
            Cmd::FeedLoaded(service.fetch_feed().await.map_err(|err| {
                let needs_sign_in = matches!(&err, BackendError::Auth(auth) if auth.needs_interaction());
                (error_text(&err), needs_sign_in)
            }))
        });
    }

    fn enter_main(&mut self, account: Account, cached: Option<Feed>, sender: &ComponentSender<Self>) {
        self.account = Some(account);
        self.page = Page::Main;
        self.sign_in = SignInState::Idle;
        self.sign_in_error = None;
        self.clear_feed();
        if let Some(feed) = cached {
            self.show_feed(feed, sender);
        }
        self.refresh(sender);
    }

    // --- sign-in ----------------------------------------------------------------

    fn abort_sign_in(&mut self) {
        if let Some(task) = self.sign_in_task.take() {
            task.abort();
        }
        self.manual = None;
    }

    fn start_sign_in(&mut self, sender: &ComponentSender<Self>) {
        let Some(service) = self.service.clone() else { return };
        self.abort_sign_in();
        self.sign_in = SignInState::Waiting;
        self.sign_in_error = None;
        self.last_url = None;
        let out = sender.command_sender().clone();
        let task = relm4::spawn(async move {
            let result = service.sign_in().await.map_err(|err| sign_in_error_text(&err));
            let _ = out.send(Cmd::SignedIn(result));
        });
        self.sign_in_task = Some(task.abort_handle());
    }

    // --- connections --------------------------------------------------------------

    fn connect(&mut self, key: String, target: Target, sender: &ComponentSender<Self>) {
        if let Some(existing) = self.sessions.get(&key)
            && !existing.stopped
        {
            if existing.connected {
                self.toast(&format!("{} is already connected.", existing.title));
            }
            return;
        }
        let title = match &target {
            Target::Resource(resource) => resource.title.clone(),
            Target::File { title, .. } => title.clone(),
        };
        self.next_generation += 1;
        self.failures.remove(&key);
        self.titles.insert(key.clone(), title.clone());
        self.logs.begin_session(&key, &title);
        self.sessions.insert(
            key.clone(),
            ActiveSession {
                generation: self.next_generation,
                title,
                target,
                handle: None,
                connected: false,
                stopped: false,
                tried_saved_password: false,
                password_stored: false,
                explanation: None,
            },
        );
        self.refresh_card(&key);
        self.launch(&key, None, sender);
    }

    fn launch(&self, key: &str, password: Option<String>, sender: &ComponentSender<Self>) {
        let (Some(service), Some(session)) = (self.service.clone(), self.sessions.get(key)) else { return };
        let generation = session.generation;
        let target = session.target.clone();
        let settings = match &target {
            Target::Resource(resource) => self.config.settings_for(&resource.key()).clone(),
            Target::File { .. } => self.config.defaults.clone(),
        };
        let scale = self.local_scale();
        let out = sender.command_sender().clone();
        let sink_key = key.to_owned();
        let events: EventSink = Arc::new(move |event: SessionEvent| {
            if matches!(event, SessionEvent::Exited { .. }) {
                LIVE_SESSIONS.fetch_sub(1, Ordering::SeqCst);
            }
            let _ = out.send(Cmd::Session { key: sink_key.clone(), generation, event });
        });
        let key = key.to_owned();
        sender.oneshot_command(async move {
            let password = password.as_deref();
            LIVE_SESSIONS.fetch_add(1, Ordering::SeqCst);
            let result = match &target {
                Target::Resource(resource) => service.connect(resource, &settings, password, scale, events).await,
                Target::File { title, text } => service.connect_rdp(text, title, &settings, password, scale, events).await,
            };
            let result = match result {
                Ok(handle) => Ok(handle),
                Err(BackendError::PasswordRequired) => {
                    LIVE_SESSIONS.fetch_sub(1, Ordering::SeqCst);
                    Err(ConnectFailure::PasswordRequired)
                }
                Err(err) => {
                    LIVE_SESSIONS.fetch_sub(1, Ordering::SeqCst);
                    Err(ConnectFailure::Other(error_text(&err)))
                }
            };
            Cmd::Launched { key, generation, result }
        });
    }

    fn disconnect(&mut self, key: &str) {
        if let Some(session) = self.sessions.get_mut(key) {
            session.stopped = true;
            if let Some(handle) = session.handle.as_mut() {
                handle.stop();
            }
        }
        self.refresh_card(key);
    }

    fn stop_all_sessions(&mut self) {
        for session in self.sessions.values_mut() {
            session.stopped = true;
            if let Some(handle) = session.handle.as_mut() {
                handle.stop();
            }
        }
    }

    fn fail(&mut self, key: &str, reason: String, sender: &ComponentSender<Self>) {
        let session = self.sessions.remove(key);
        let is_file = matches!(session.as_ref().map(|s| &s.target), Some(Target::File { .. }));
        self.failures.insert(key.to_owned(), reason.clone());
        self.refresh_card(key);
        if is_file || self.feed.as_ref().and_then(|f| f.find(key)).is_none() {
            let toast = adw::Toast::builder()
                .title(gtk::glib::markup_escape_text(&reason))
                .button_label("View Log")
                .timeout(10)
                .build();
            let sender = sender.clone();
            let key = key.to_owned();
            toast.connect_button_clicked(move |_| sender.input(AppMsg::Card(CardOutput::ViewLog(key.clone()))));
            self.toaster.add_toast(toast);
        }
    }

    fn ask_password(&self, key: &str, sender: &ComponentSender<Self>) {
        let storage = self.service.as_ref().map(Service::secret_storage).unwrap_or("the keyring");
        let sender = sender.clone();
        let key = key.to_owned();
        dialogs::ask_password(&self.window, &self.title_of(&key), storage, move |answer| match answer {
            Some((password, remember)) => sender.input(AppMsg::PasswordGiven { key: key.clone(), password, remember }),
            None => sender.input(AppMsg::PasswordCancelled(key.clone())),
        });
    }

    fn handle_session_event(&mut self, key: String, generation: u64, event: SessionEvent, sender: &ComponentSender<Self>) {
        let current = self.sessions.get(&key).is_some_and(|s| s.generation == generation);
        match event {
            SessionEvent::Log(line) => {
                if current
                    && let Some(explanation) = freerdp::explain_log_line(&line)
                {
                    self.sessions.get_mut(&key).unwrap().explanation = Some(explanation);
                }
                self.logs.push(&key, line.clone());
                self.log_viewer.emit(LogViewerMsg::Append { key, line });
            }
            SessionEvent::Connected if current => {
                let session = self.sessions.get_mut(&key).unwrap();
                if !session.connected {
                    session.connected = true;
                    if matches!(session.target, Target::File { .. }) {
                        let text = format!("{} is connected.", session.title);
                        self.toast(&text);
                    }
                }
                self.refresh_card(&key);
            }
            SessionEvent::Exited { code } if current => {
                self.logs.push(&key, exit_line(code));
                self.log_viewer.emit(LogViewerMsg::Append { key: key.clone(), line: exit_line(code) });
                let session = self.sessions.get(&key).unwrap();
                let exit = ExitInfo {
                    connected: session.connected,
                    stopped_by_user: session.stopped,
                    code,
                    explanation: session.explanation,
                };
                match logic::state_after_exit(&exit) {
                    CardState::Failed(mut reason) => {
                        if session.password_stored && logic::is_credential_failure(code, session.explanation) {
                            reason.push_str(" The saved password was removed; you will be asked for it next time.");
                            if let (Some(service), Some(account)) = (self.service.clone(), self.account.clone()) {
                                let key = key.clone();
                                sender.oneshot_command(async move {
                                    service.forget_password(&account.id(), &key).await;
                                    Cmd::Nothing
                                });
                            }
                        }
                        self.fail(&key, reason, sender);
                    }
                    _ => {
                        self.sessions.remove(&key);
                        self.refresh_card(&key);
                    }
                }
            }
            _ => {}
        }
    }

    fn open_settings(&self, target: SettingsTarget, sender: &ComponentSender<Self>) {
        let initial = match &target {
            SettingsTarget::Defaults => self.config.defaults.clone(),
            SettingsTarget::Resource { key, .. } => self.config.settings_for(key).clone(),
        };
        let paths_sender = sender.clone();
        let context = SettingsContext {
            target: target.clone(),
            local_scale: self.local_scale(),
            clients: self.clients.clone(),
            client_paths: (target == SettingsTarget::Defaults)
                .then(|| (self.config.sdl_client_path.clone(), self.config.x11_client_path.clone())),
            on_paths: std::rc::Rc::new(move |sdl, x11| paths_sender.input(AppMsg::ClientPathsChanged(sdl, x11))),
        };
        let sender = sender.clone();
        let dialog = settings::build(context, initial, move |settings| {
            sender.input(AppMsg::SettingsChanged(target.clone(), settings));
        });
        dialog.present(Some(&self.window));
    }

    fn open_url(&self, url: &str, sender: &ComponentSender<Self>) {
        let sender = sender.clone();
        gtk::UriLauncher::new(url).launch(Some(&self.window), None::<&gtk::gio::Cancellable>, move |result| {
            if let Err(err) = result {
                sender.input(AppMsg::Status(format!(
                    "The browser could not be opened ({err}). Use Copy Link and open the address yourself."
                )));
            }
        });
    }
}

impl Component for App {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = Cmd;
    type Root = adw::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title(crate::consts::APP_NAME)
            .default_width(1100)
            .default_height(760)
            .width_request(360)
            .height_request(400)
            .build()
    }

    fn init(init: Self::Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        relm4::set_global_css(view::CSS);
        view::add_source_icons();
        let config_path =
            if init.demo { paths::config_dir().join("demo-config.json") } else { paths::config_file() };
        let config = Config::load_from(&config_path);

        let toaster = Toaster::default();
        let sections_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let widgets = view::build(&root, toaster.overlay_widget(), &sections_box, &sender);
        let log_viewer = LogViewer::builder().launch(root.clone().upcast()).detach();
        {
            let sender = sender.clone();
            root.connect_close_request(move |_| {
                sender.input(AppMsg::Closing);
                gtk::glib::Propagation::Proceed
            });
        }

        let model = App {
            window: root.clone(),
            toaster,
            demo: init.demo,
            config,
            config_path,
            service: None,
            clients: Clients::default(),
            page: Page::Loading,
            startup_error: None,
            sign_in: SignInState::Idle,
            sign_in_error: None,
            sign_in_task: None,
            manual: None,
            last_url: None,
            account: None,
            feed: None,
            feed_loading: false,
            feed_error: None,
            query: String::new(),
            sections: Vec::new(),
            sections_box,
            icons: HashMap::new(),
            sessions: HashMap::new(),
            failures: HashMap::new(),
            titles: HashMap::new(),
            logs: LogStore::default(),
            log_viewer,
            next_generation: 0,
        };
        widgets.sync(&model);

        // The backend's callbacks run on runtime threads; hop to this one.
        let input = sender.input_sender().clone();
        let open_url: OpenUrl = Arc::new(move |url: &str| {
            let _ = input.send(AppMsg::OpenUrl(url.to_owned()));
        });
        let input = sender.input_sender().clone();
        let status: StatusSink = Arc::new(move |text: String| {
            let _ = input.send(AppMsg::Status(text));
        });
        let (sdl, x11) = (model.config.sdl_client_path.clone(), model.config.x11_client_path.clone());
        let demo = init.demo;
        sender.oneshot_command(async move {
            if demo {
                return Cmd::Started(Ok(Service::Demo(Arc::new(Demo::from_env()))));
            }
            Cmd::Started(Backend::start(open_url, status, &sdl, &x11).await.map(Service::Real))
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            AppMsg::OpenUrl(url) => {
                self.open_url(&url, &sender);
                self.last_url = Some(url);
            }
            AppMsg::Status(text) => self.toast(&text),
            AppMsg::SignIn => self.start_sign_in(&sender),
            AppMsg::CancelSignIn => {
                self.abort_sign_in();
                self.sign_in = SignInState::Idle;
            }
            AppMsg::UseManualSignIn => {
                let Some(service) = self.service.clone() else { return };
                self.abort_sign_in();
                self.manual = Some(Arc::new(service.start_manual_sign_in()));
                self.sign_in = SignInState::Manual;
                self.sign_in_error = None;
            }
            AppMsg::SubmitPasted(text) => {
                let (Some(service), Some(pending)) = (self.service.clone(), self.manual.clone()) else { return };
                if text.trim().is_empty() || self.sign_in == SignInState::ManualSubmitting {
                    return;
                }
                self.sign_in = SignInState::ManualSubmitting;
                self.sign_in_error = None;
                sender.oneshot_command(async move {
                    Cmd::SignedIn(service.finish_manual_sign_in(&pending, text.trim()).await.map_err(|e| sign_in_error_text(&e)))
                });
            }
            AppMsg::CopySignInLink => match &self.last_url {
                Some(url) => {
                    self.window.clipboard().set_text(url);
                    self.toast("Sign-in address copied. Paste it into your browser.");
                }
                None => self.toast("The sign-in address is not ready yet."),
            },
            AppMsg::Refresh => self.refresh(&sender),
            AppMsg::SearchChanged(query) => {
                self.query = query;
                for section in &self.sections {
                    for index in 0..section.entries.len() {
                        section.cards.send(index, CardMsg::Filter(self.query.clone()));
                    }
                    self.apply_filter_to(section);
                }
            }
            AppMsg::Card(output) => match output {
                CardOutput::Connect(key) => {
                    if let Some(resource) = self.feed.as_ref().and_then(|f| f.find(&key)).cloned() {
                        self.connect(key, Target::Resource(resource), &sender);
                    }
                }
                CardOutput::Disconnect(key) => self.disconnect(&key),
                CardOutput::Settings(key) => {
                    let title = self.title_of(&key);
                    self.open_settings(SettingsTarget::Resource { key, title }, &sender);
                }
                CardOutput::Reset(key) => {
                    if self.config.overrides.remove(&key).is_some() {
                        self.save_config();
                        self.send_card(&key, || CardMsg::SetOverride(false));
                        self.toast(&format!("{} uses the default settings again.", self.title_of(&key)));
                    }
                }
                CardOutput::ViewLog(key) => {
                    self.log_viewer.emit(LogViewerMsg::Show {
                        title: self.title_of(&key),
                        lines: self.logs.lines(&key),
                        file: self.logs.file(&key).map(PathBuf::from),
                        key,
                    });
                }
            },
            AppMsg::ShowDefaults => self.open_settings(SettingsTarget::Defaults, &sender),
            AppMsg::SettingsChanged(target, settings) => {
                match target {
                    SettingsTarget::Defaults => self.config.defaults = settings,
                    SettingsTarget::Resource { key, .. } => {
                        let new = !self.config.has_override(&key);
                        self.config.overrides.insert(key.clone(), settings);
                        if new {
                            self.send_card(&key, || CardMsg::SetOverride(true));
                        }
                    }
                }
                self.save_config();
            }
            AppMsg::ClientPathsChanged(sdl, x11) => {
                self.config.sdl_client_path = sdl.clone();
                self.config.x11_client_path = x11.clone();
                self.save_config();
                if let Some(service) = self.service.clone() {
                    sender.oneshot_command(async move { Cmd::ClientsDetected(service.redetect_clients(&sdl, &x11).await) });
                }
            }
            AppMsg::OpenRdpFile => {
                let sender = sender.clone();
                dialogs::choose_rdp_file(&self.window, move |path| sender.input(AppMsg::RdpFileChosen(path)));
            }
            AppMsg::RdpFileChosen(path) => match std::fs::read(&path) {
                Ok(bytes) => {
                    let title = rdp_file_title(&path);
                    let text = crate::feed::client::decode_text(&bytes);
                    let key = format!("file:{}", path.display());
                    self.toast(&format!("Connecting to {title}…"));
                    self.connect(key, Target::File { title, text }, &sender);
                }
                Err(err) => self.toast(&format!("{} could not be read: {err}.", path.display())),
            },
            AppMsg::OpenLogsFolder => {
                let dir = self.service.as_ref().map(Service::logs_dir).unwrap_or_else(paths::logs_dir);
                let _ = std::fs::create_dir_all(&dir);
                let launcher = gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&dir)));
                let sender = sender.clone();
                launcher.launch(Some(&self.window), None::<&gtk::gio::Cancellable>, move |result| {
                    if let Err(err) = result {
                        sender.input(AppMsg::Status(format!("The logs folder {} could not be opened: {err}.", dir.display())));
                    }
                });
            }
            AppMsg::ShowAbout => dialogs::about(&self.window),
            AppMsg::SignOut => {
                let Some(service) = self.service.clone() else { return };
                self.stop_all_sessions();
                self.sessions.clear();
                self.failures.clear();
                self.logs = LogStore::default();
                self.icons.clear();
                self.titles.clear();
                self.clear_feed();
                self.account = None;
                if !self.demo {
                    self.config.account = None;
                    self.save_config();
                }
                self.page = Page::SignIn;
                self.sign_in = SignInState::Idle;
                self.sign_in_error = None;
                sender.oneshot_command(async move {
                    service.sign_out().await;
                    Cmd::Toast("Signed out. The saved sign-in was removed from this computer.".into())
                });
            }
            AppMsg::PasswordGiven { key, password, remember } => {
                if self.sessions.get(&key).is_none_or(|s| s.stopped) {
                    if self.sessions.remove(&key).is_some() {
                        self.refresh_card(&key);
                    }
                    return;
                }
                if remember {
                    if let Some(session) = self.sessions.get_mut(&key) {
                        session.password_stored = true;
                    }
                    if let (Some(service), Some(account)) = (self.service.clone(), self.account.clone()) {
                        let (key, password) = (key.clone(), password.clone());
                        sender.oneshot_command(async move {
                            match service.remember_password(&account.id(), &key, &password).await {
                                Ok(()) => Cmd::Nothing,
                                Err(err) => Cmd::Toast(format!("The password could not be saved: {err}.")),
                            }
                        });
                    }
                }
                self.launch(&key, Some(password), &sender);
            }
            AppMsg::PasswordCancelled(key) => {
                self.sessions.remove(&key);
                self.refresh_card(&key);
            }
            AppMsg::Closing => {
                self.abort_sign_in();
                self.stop_all_sessions();
            }
        }
    }

    fn update_cmd(&mut self, message: Self::CommandOutput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            Cmd::Started(Err(err)) => {
                self.page = Page::StartupFailed;
                self.startup_error = Some(format!("{}.", err.trim_end_matches('.')));
            }
            Cmd::Started(Ok(service)) => {
                self.clients = service.clients();
                self.service = Some(service.clone());
                match self.config.account.clone().filter(|_| !self.demo) {
                    Some(account) => sender.oneshot_command(async move {
                        let ok = service.restore_account(&account).await;
                        let cached = if ok { service.cached_feed(&account) } else { None };
                        Cmd::Restored { account, ok, cached }
                    }),
                    None => self.page = Page::SignIn,
                }
            }
            Cmd::Restored { account, ok: true, cached } => self.enter_main(account, cached, &sender),
            Cmd::Restored { account, ok: false, .. } => {
                self.page = Page::SignIn;
                self.sign_in_error = Some(format!(
                    "The saved sign-in for {} was not found in the keyring. Sign in again.",
                    account.username
                ));
            }
            Cmd::SignedIn(Ok(account)) => {
                self.sign_in_task = None;
                self.manual = None;
                if !self.demo {
                    self.config.account = Some(account.clone());
                    self.save_config();
                }
                let cached = self.service.as_ref().and_then(|s| s.cached_feed(&account));
                self.enter_main(account, cached, &sender);
            }
            Cmd::SignedIn(Err(text)) => {
                self.sign_in_task = None;
                self.sign_in = if self.sign_in == SignInState::ManualSubmitting { SignInState::Manual } else { SignInState::Idle };
                if self.sign_in == SignInState::Idle {
                    self.manual = None;
                }
                self.sign_in_error = Some(text);
            }
            Cmd::FeedLoaded(result) => {
                self.feed_loading = false;
                if self.account.is_none() {
                    return;
                }
                match result {
                    Ok(feed) => {
                        self.feed_error = None;
                        self.show_feed(feed, &sender);
                    }
                    Err((text, true)) => {
                        self.page = Page::SignIn;
                        self.sign_in = SignInState::Idle;
                        self.sign_in_error = Some(format!("Your sign-in has expired. Sign in again. ({text})"));
                    }
                    Err((text, false)) if self.feed.is_some() => {
                        self.toast(&format!("Could not refresh: {text}"));
                    }
                    Err((text, false)) => self.feed_error = Some(text),
                }
            }
            Cmd::Icon { key, path } => {
                self.icons.insert(key.clone(), path.clone());
                self.send_card(&key, || CardMsg::SetIcon(path.clone()));
            }
            Cmd::Launched { key, generation, result } => {
                let current = self.sessions.get(&key).is_some_and(|s| s.generation == generation);
                match result {
                    Ok(mut handle) if !current => handle.stop(),
                    Ok(mut handle) => {
                        let file = handle.log_file();
                        self.logs.set_file(&key, file.clone());
                        self.log_viewer.emit(LogViewerMsg::SetFile { key: key.clone(), file });
                        let session = self.sessions.get_mut(&key).unwrap();
                        if session.stopped {
                            handle.stop();
                        }
                        session.handle = Some(handle);
                    }
                    Err(_) if !current => {}
                    Err(ConnectFailure::PasswordRequired) => {
                        let session = self.sessions.get_mut(&key).unwrap();
                        if session.stopped {
                            self.sessions.remove(&key);
                            self.refresh_card(&key);
                        } else if !session.tried_saved_password && self.account.is_some() {
                            session.tried_saved_password = true;
                            let service = self.service.clone().unwrap();
                            let account = self.account.as_ref().unwrap().id();
                            sender.oneshot_command(async move {
                                let password = service.saved_password(&account, &key).await;
                                Cmd::SavedPassword { key, generation, password }
                            });
                        } else {
                            self.ask_password(&key, &sender);
                        }
                    }
                    Err(ConnectFailure::Other(text)) => {
                        if self.sessions.get(&key).is_some_and(|s| s.stopped) {
                            self.sessions.remove(&key);
                            self.refresh_card(&key);
                        } else {
                            self.logs.push(&key, text.clone());
                            self.fail(&key, text, &sender);
                        }
                    }
                }
            }
            Cmd::SavedPassword { key, generation, password } => {
                let Some(session) = self.sessions.get_mut(&key).filter(|s| s.generation == generation) else { return };
                if session.stopped {
                    self.sessions.remove(&key);
                    self.refresh_card(&key);
                    return;
                }
                match password {
                    Some(password) => {
                        session.password_stored = true;
                        self.launch(&key, Some(password), &sender);
                    }
                    None => self.ask_password(&key, &sender),
                }
            }
            Cmd::Session { key, generation, event } => self.handle_session_event(key, generation, event, &sender),
            Cmd::ClientsDetected(clients) => self.clients = clients,
            Cmd::Toast(text) => self.toast(&text),
            Cmd::Nothing => {}
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        widgets.sync(self);
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        self.stop_all_sessions();
    }
}
