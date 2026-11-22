//! One desktop or app in the grid (a Relm4 factory component).

use std::path::PathBuf;

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::gtk::gio;

use super::logic::{CardState, matches_search};
use crate::feed::{Resource, ResourceKind};

pub const CARD_WIDTH: i32 = 208;
const ICON_SIZE: i32 = 64;
/// Fixed text width, so every card in the grid has the same width.
const TITLE_CHARS: i32 = 20;

pub struct CardInit {
    pub resource: Resource,
    pub state: CardState,
    pub has_override: bool,
    pub icon: Option<PathBuf>,
    pub query: String,
}

pub struct Card {
    pub key: String,
    resource: Resource,
    state: CardState,
    has_override: bool,
    icon: Option<PathBuf>,
    visible: bool,
}

#[derive(Debug)]
pub enum CardMsg {
    SetState(CardState),
    SetIcon(PathBuf),
    SetOverride(bool),
    Filter(String),
}

#[derive(Debug)]
pub enum CardOutput {
    Connect(String),
    Disconnect(String),
    Settings(String),
    Reset(String),
    ViewLog(String),
}

pub struct CardWidgets {
    child: gtk::FlowBoxChild,
    icon: gtk::Image,
    applied_icon: Option<PathBuf>,
    main_button: gtk::Button,
    menu_button: gtk::MenuButton,
    menu_override: Option<bool>,
    status: gtk::Stack,
    reason: gtk::Label,
    actions: gio::SimpleActionGroup,
}

impl Card {
    pub fn matches(&self, query: &str) -> bool {
        matches_search(query, &self.resource.title, &self.resource.workspace)
    }

    fn menu(&self) -> gio::Menu {
        let menu = gio::Menu::new();
        let main = gio::Menu::new();
        main.append(Some("Settings for this desktop…"), Some("card.settings"));
        if self.has_override {
            main.append(Some("Reset to defaults"), Some("card.reset"));
        }
        menu.append_section(None, &main);
        let session = gio::Menu::new();
        session.append(Some("View log"), Some("card.log"));
        session.append(Some("Disconnect"), Some("card.disconnect"));
        menu.append_section(None, &session);
        menu
    }
}

fn icon_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Desktop => "computer-symbolic",
        ResourceKind::RemoteApp => "application-x-executable-symbolic",
    }
}

impl FactoryComponent for Card {
    type ParentWidget = gtk::FlowBox;
    type CommandOutput = ();
    type Input = CardMsg;
    type Output = CardOutput;
    type Init = CardInit;
    type Root = gtk::Box;
    type Widgets = CardWidgets;
    type Index = DynamicIndex;

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        let mut card = Self {
            key: init.resource.key(),
            resource: init.resource,
            state: init.state,
            has_override: init.has_override,
            icon: init.icon,
            visible: true,
        };
        card.visible = card.matches(&init.query);
        card
    }

    fn init_root(&self) -> Self::Root {
        gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(CARD_WIDTH)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .css_classes(["card", "resource-card"])
            .build()
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        returned_widget: &gtk::FlowBoxChild,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        returned_widget.set_focusable(false);
        let kind_label = self.resource.kind.label();

        // The large clickable area: icon, title, kind.
        let icon = gtk::Image::builder().pixel_size(ICON_SIZE).icon_name(icon_name(self.resource.kind)).build();
        icon.add_css_class("resource-icon");
        let title = gtk::Label::builder()
            .label(&self.resource.title)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .justify(gtk::Justification::Center)
            .width_chars(TITLE_CHARS)
            .max_width_chars(TITLE_CHARS)
            .css_classes(["heading"])
            .build();
        let kind = gtk::Label::builder().label(kind_label).css_classes(["dim-label", "caption"]).build();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        content.append(&icon);
        content.append(&title);
        content.append(&kind);
        let main_button = gtk::Button::builder().child(&content).css_classes(["flat", "card-main"]).build();
        main_button.set_tooltip_text(Some(&format!("Connect to {}", self.resource.title)));
        main_button.update_property(&[gtk::accessible::Property::Label(&format!(
            "{} ({kind_label}, {})",
            self.resource.title, self.resource.workspace
        ))]);
        {
            let sender = sender.clone();
            let key = self.key.clone();
            main_button.connect_clicked(move |_| {
                let _ = sender.output(CardOutput::Connect(key.clone()));
            });
        }

        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat", "circular", "card-menu"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_end(6)
            .tooltip_text("More options")
            .build();
        let overlay = gtk::Overlay::builder().child(&main_button).build();
        overlay.add_overlay(&menu_button);

        // The state line under the main area.
        let status = gtk::Stack::builder().vhomogeneous(false).transition_type(gtk::StackTransitionType::Crossfade).build();
        status.add_named(&gtk::Box::new(gtk::Orientation::Vertical, 0), Some("idle"));

        let connecting = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_bottom(12).build();
        connecting.append(&adw::Spinner::new());
        connecting.append(&gtk::Label::new(Some("Connecting…")));
        status.add_named(&connecting, Some("connecting"));

        let connected = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_bottom(10).build();
        let badge = gtk::Label::builder().label("Connected").css_classes(["badge", "success"]).build();
        let disconnect = gtk::Button::builder().label("Disconnect").css_classes(["pill", "small-pill"]).build();
        connected.append(&badge);
        connected.append(&disconnect);
        status.add_named(&connected, Some("connected"));

        let failed = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(10)
            .build();
        let reason = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .justify(gtk::Justification::Center)
            .width_chars(TITLE_CHARS)
            .max_width_chars(TITLE_CHARS)
            .selectable(true)
            .css_classes(["error", "caption"])
            .build();
        let view_log = gtk::Button::builder().label("View log").halign(gtk::Align::Center).css_classes(["flat", "small-pill"]).build();
        failed.append(&reason);
        failed.append(&view_log);
        status.add_named(&failed, Some("failed"));

        root.append(&overlay);
        root.append(&status);

        for (button, make) in [
            (&disconnect, CardOutput::Disconnect as fn(String) -> CardOutput),
            (&view_log, CardOutput::ViewLog as fn(String) -> CardOutput),
        ] {
            let sender = sender.clone();
            let key = self.key.clone();
            button.connect_clicked(move |_| {
                let _ = sender.output(make(key.clone()));
            });
        }

        // The per-card menu's actions.
        let actions = gio::SimpleActionGroup::new();
        for (name, make) in [
            ("settings", CardOutput::Settings as fn(String) -> CardOutput),
            ("reset", CardOutput::Reset),
            ("log", CardOutput::ViewLog),
            ("disconnect", CardOutput::Disconnect),
        ] {
            let action = gio::SimpleAction::new(name, None);
            let sender = sender.clone();
            let key = self.key.clone();
            action.connect_activate(move |_, _| {
                let _ = sender.output(make(key.clone()));
            });
            actions.add_action(&action);
        }
        root.insert_action_group("card", Some(&actions));

        let mut widgets = CardWidgets {
            child: returned_widget.clone(),
            icon,
            applied_icon: None,
            main_button,
            menu_button,
            menu_override: None,
            status,
            reason,
            actions,
        };
        self.update_view(&mut widgets, sender);
        widgets
    }

    fn update(&mut self, message: Self::Input, _sender: FactorySender<Self>) {
        match message {
            CardMsg::SetState(state) => self.state = state,
            CardMsg::SetIcon(path) => self.icon = Some(path),
            CardMsg::SetOverride(value) => self.has_override = value,
            CardMsg::Filter(query) => self.visible = self.matches(&query),
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        widgets.child.set_visible(self.visible);
        if self.icon.is_some() && widgets.applied_icon != self.icon {
            widgets.icon.set_from_file(self.icon.as_deref());
            widgets.applied_icon = self.icon.clone();
        }
        let page = match &self.state {
            CardState::Idle => "idle",
            CardState::Connecting => "connecting",
            CardState::Connected => "connected",
            CardState::Failed(reason) => {
                widgets.reason.set_label(reason);
                "failed"
            }
        };
        widgets.status.set_visible_child_name(page);
        if widgets.menu_override != Some(self.has_override) {
            widgets.menu_button.set_menu_model(Some(&self.menu()));
            widgets.menu_override = Some(self.has_override);
        }
        let active = self.state.is_active();
        if let Some(action) = widgets.actions.lookup_action("disconnect") {
            action.downcast::<gio::SimpleAction>().unwrap().set_enabled(active);
        }
        widgets.main_button.set_tooltip_text(Some(&match &self.state {
            CardState::Connected => format!("{} is connected", self.resource.title),
            CardState::Connecting => format!("Connecting to {}…", self.resource.title),
            _ => format!("Connect to {}", self.resource.title),
        }));
    }
}
