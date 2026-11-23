//! Floating top-center connection bar for active non-RemoteApp SDL sessions.
//!
//! Uses `gtk4-layer-shell` to render a single shared overlay bar at the top edge
//! of the screen without stealing keyboard focus from the remote desktop.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use relm4::ComponentSender;

use super::app::{App, AppMsg};
use super::card::CardOutput;
use super::layer_shell;
use crate::config::ConnectionBar;
use crate::consts;

const COLLAPSE_LEAVE_MS: u64 = 400;
const COLLAPSE_INITIAL_MS: u64 = 2500;

#[derive(Debug, Clone)]
struct SessionEntry {
    key: String,
    title: String,
    is_file: bool,
    connected: bool,
    hotkey_hint: Option<&'static str>,
}

#[derive(Default)]
struct BarData {
    entries: Vec<SessionEntry>,
    active_key: Option<String>,
    pinned: bool,
    collapse_timer: Option<gtk::glib::SourceId>,
}

#[derive(Clone)]
struct BarUi {
    window: gtk::Window,
    stack: gtk::Stack,
    pin_button: gtk::ToggleButton,
    dot: gtk::Box,
    title_label: gtk::Label,
    switch_button: gtk::Button,
    settings_button: gtk::Button,
    motion: gtk::EventControllerMotion,
}

#[derive(Clone)]
struct WeakBarUi {
    window: gtk::glib::WeakRef<gtk::Window>,
    stack: gtk::glib::WeakRef<gtk::Stack>,
    pin_button: gtk::glib::WeakRef<gtk::ToggleButton>,
    dot: gtk::glib::WeakRef<gtk::Box>,
    title_label: gtk::glib::WeakRef<gtk::Label>,
    switch_button: gtk::glib::WeakRef<gtk::Button>,
    settings_button: gtk::glib::WeakRef<gtk::Button>,
    motion: gtk::glib::WeakRef<gtk::EventControllerMotion>,
}

impl BarUi {
    fn downgrade(&self) -> WeakBarUi {
        WeakBarUi {
            window: self.window.downgrade(),
            stack: self.stack.downgrade(),
            pin_button: self.pin_button.downgrade(),
            dot: self.dot.downgrade(),
            title_label: self.title_label.downgrade(),
            switch_button: self.switch_button.downgrade(),
            settings_button: self.settings_button.downgrade(),
            motion: self.motion.downgrade(),
        }
    }
}

impl WeakBarUi {
    fn upgrade(&self) -> Option<BarUi> {
        Some(BarUi {
            window: self.window.upgrade()?,
            stack: self.stack.upgrade()?,
            pin_button: self.pin_button.upgrade()?,
            dot: self.dot.upgrade()?,
            title_label: self.title_label.upgrade()?,
            switch_button: self.switch_button.upgrade()?,
            settings_button: self.settings_button.upgrade()?,
            motion: self.motion.upgrade()?,
        })
    }
}

/// Manages the optional Wayland overlay connection bar across active sessions.
pub struct SessionBar {
    sender: ComponentSender<App>,
    data: Rc<RefCell<BarData>>,
    ui: Option<BarUi>,
}

impl SessionBar {
    pub fn new(sender: ComponentSender<App>) -> Self {
        Self {
            sender,
            data: Rc::new(RefCell::new(BarData::default())),
            ui: None,
        }
    }

    pub fn add(
        &mut self,
        key: String,
        title: String,
        is_file: bool,
        mode: ConnectionBar,
        hotkey_hint: Option<&'static str>,
    ) {
        if mode == ConnectionBar::Off || !layer_shell::is_supported() {
            return;
        }

        let created = if self.ui.is_none() {
            let Some(ui) = build_overlay(&self.sender, &self.data) else {
                return;
            };
            self.ui = Some(ui);
            true
        } else {
            false
        };

        {
            let mut data = self.data.borrow_mut();
            if let Some(existing) = data.entries.iter_mut().find(|e| e.key == key) {
                existing.title = title;
                existing.is_file = is_file;
                existing.connected = false;
                existing.hotkey_hint = hotkey_hint;
            } else {
                data.entries.push(SessionEntry {
                    key: key.clone(),
                    title,
                    is_file,
                    connected: false,
                    hotkey_hint,
                });
            }
            data.active_key = Some(key);
            if created {
                data.pinned = mode == ConnectionBar::Pinned;
            } else if mode == ConnectionBar::Pinned {
                data.pinned = true;
            }
        }

        if let Some(ui) = &self.ui {
            sync_ui(ui, &self.data);
            expand(ui, &self.data);
            ui.window.present();
            if !self.data.borrow().pinned {
                schedule_collapse(ui, &self.data, COLLAPSE_INITIAL_MS);
            }
        }
    }

    pub fn set_connected(&mut self, key: &str) {
        let updated = {
            let mut data = self.data.borrow_mut();
            if let Some(entry) = data.entries.iter_mut().find(|e| e.key == key) {
                entry.connected = true;
                true
            } else {
                false
            }
        };
        if updated && let Some(ui) = &self.ui {
            sync_ui(ui, &self.data);
        }
    }

    pub fn remove(&mut self, key: &str) {
        let empty = {
            let mut data = self.data.borrow_mut();
            let before = data.entries.len();
            data.entries.retain(|e| e.key != key);
            if data.entries.len() == before {
                return;
            }
            if data.active_key.as_deref() == Some(key) {
                data.active_key = data.entries.last().map(|e| e.key.clone());
            }
            data.entries.is_empty()
        };

        if empty {
            self.close_overlay();
        } else if let Some(ui) = &self.ui {
            sync_ui(ui, &self.data);
        }
    }

    pub fn clear(&mut self) {
        {
            let mut data = self.data.borrow_mut();
            data.entries.clear();
            data.active_key = None;
        }
        self.close_overlay();
    }

    fn close_overlay(&mut self) {
        cancel_collapse(&self.data);
        if let Some(ui) = self.ui.take() {
            ui.window.close();
        }
    }
}

impl Drop for SessionBar {
    fn drop(&mut self) {
        self.clear();
    }
}

fn build_overlay(sender: &ComponentSender<App>, data: &Rc<RefCell<BarData>>) -> Option<BarUi> {
    let window = gtk::Window::builder()
        .decorated(false)
        .resizable(false)
        .can_focus(false)
        .focusable(false)
        .default_width(1)
        .default_height(1)
        .css_classes(["session-bar-window"])
        .build();

    if !layer_shell::init_overlay_top_bar(&window, consts::APP_ID) {
        window.close();
        return None;
    }

    let handle = gtk::Box::builder()
        .width_request(140)
        .height_request(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .css_classes(["session-bar-handle"])
        .build();

    let pin_button = gtk::ToggleButton::builder()
        .icon_name("view-pin-symbolic")
        .tooltip_text("Pin connection bar")
        .css_classes(["flat", "circular"])
        .build();

    let dot = gtk::Box::builder()
        .width_request(8)
        .height_request(8)
        .valign(gtk::Align::Center)
        .css_classes(["session-bar-dot"])
        .build();

    let title_label = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(28)
        .css_classes(["session-bar-title"])
        .build();

    let switch_button = gtk::Button::builder()
        .visible(false)
        .css_classes(["flat", "session-bar-count"])
        .build();

    let separator = gtk::Separator::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let home_button = gtk::Button::builder()
        .icon_name("go-home-symbolic")
        .tooltip_text("Return to CloudRDPApp")
        .css_classes(["flat", "circular"])
        .build();

    let settings_button = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings for this desktop")
        .css_classes(["flat", "circular"])
        .build();

    let log_button = gtk::Button::builder()
        .icon_name("utilities-terminal-symbolic")
        .tooltip_text("View session log")
        .css_classes(["flat", "circular"])
        .build();

    let disconnect_button = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Disconnect")
        .css_classes(["destructive-action", "circular", "session-bar-disconnect"])
        .build();

    let pill = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .css_classes(["session-bar-pill"])
        .build();
    pill.append(&pin_button);
    pill.append(&dot);
    pill.append(&title_label);
    pill.append(&switch_button);
    pill.append(&separator);
    pill.append(&home_button);
    pill.append(&settings_button);
    pill.append(&log_button);
    pill.append(&disconnect_button);

    let stack = gtk::Stack::builder()
        .vhomogeneous(false)
        .hhomogeneous(false)
        .transition_type(gtk::StackTransitionType::None)
        .build();
    stack.add_named(&handle, Some("handle"));
    stack.add_named(&pill, Some("bar"));
    stack.set_visible_child_name("bar");

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build();
    root.append(&stack);

    let motion = gtk::EventControllerMotion::new();
    root.add_controller(motion.clone());
    window.set_child(Some(&root));

    let ui = BarUi {
        window,
        stack,
        pin_button,
        dot,
        title_label,
        switch_button,
        settings_button: settings_button.clone(),
        motion,
    };

    wire_signals(&ui, &home_button, &settings_button, &log_button, &disconnect_button, sender, data);
    Some(ui)
}

fn wire_signals(
    ui: &BarUi,
    home_button: &gtk::Button,
    settings_button: &gtk::Button,
    log_button: &gtk::Button,
    disconnect_button: &gtk::Button,
    sender: &ComponentSender<App>,
    data: &Rc<RefCell<BarData>>,
) {
    let weak_ui = ui.downgrade();

    {
        let weak_ui = weak_ui.clone();
        let data_c = data.clone();
        ui.motion.connect_enter(move |_, _, _| {
            if let Some(ui) = weak_ui.upgrade() {
                expand(&ui, &data_c);
            }
        });
    }

    {
        let weak_ui = weak_ui.clone();
        let data_c = data.clone();
        ui.motion.connect_leave(move |_| {
            if let Some(ui) = weak_ui.upgrade()
                && !data_c.borrow().pinned
            {
                schedule_collapse(&ui, &data_c, COLLAPSE_LEAVE_MS);
            }
        });
    }

    {
        let weak_ui = weak_ui.clone();
        let data_c = data.clone();
        ui.pin_button.connect_toggled(move |btn| {
            let active = btn.is_active();
            if data_c.borrow().pinned == active {
                return;
            }
            data_c.borrow_mut().pinned = active;
            btn.set_tooltip_text(Some(if active {
                "Unpin connection bar (auto-hide)"
            } else {
                "Pin connection bar"
            }));
            if let Some(ui) = weak_ui.upgrade() {
                if active {
                    expand(&ui, &data_c);
                } else if !ui.motion.contains_pointer() {
                    schedule_collapse(&ui, &data_c, COLLAPSE_LEAVE_MS);
                }
            }
        });
    }

    {
        let weak_ui = weak_ui;
        let data_c = data.clone();
        ui.switch_button.connect_clicked(move |_| {
            {
                let mut d = data_c.borrow_mut();
                if d.entries.len() > 1 {
                    let current = d
                        .active_key
                        .as_deref()
                        .and_then(|k| d.entries.iter().position(|e| e.key == k))
                        .unwrap_or(0);
                    let next = (current + 1) % d.entries.len();
                    d.active_key = Some(d.entries[next].key.clone());
                }
            }
            if let Some(ui) = weak_ui.upgrade() {
                sync_ui(&ui, &data_c);
            }
        });
    }

    {
        let sender = sender.clone();
        home_button.connect_clicked(move |_| sender.input(AppMsg::RaiseWindow));
    }

    {
        let sender = sender.clone();
        let data_c = data.clone();
        settings_button.connect_clicked(move |_| {
            if let Some(entry) = active_entry(&data_c.borrow()) {
                sender.input(AppMsg::RaiseWindow);
                if entry.is_file {
                    sender.input(AppMsg::ShowDefaults);
                } else {
                    sender.input(AppMsg::Card(CardOutput::Settings(entry.key)));
                }
            }
        });
    }

    {
        let sender = sender.clone();
        let data_c = data.clone();
        log_button.connect_clicked(move |_| {
            if let Some(entry) = active_entry(&data_c.borrow()) {
                sender.input(AppMsg::Card(CardOutput::ViewLog(entry.key)));
            }
        });
    }

    {
        let sender = sender.clone();
        let data_c = data.clone();
        disconnect_button.connect_clicked(move |_| {
            if let Some(entry) = active_entry(&data_c.borrow()) {
                sender.input(AppMsg::Card(CardOutput::Disconnect(entry.key)));
            }
        });
    }
}

fn active_entry(data: &BarData) -> Option<SessionEntry> {
    data.active_key
        .as_deref()
        .and_then(|k| data.entries.iter().find(|e| e.key == k))
        .or_else(|| data.entries.last())
        .cloned()
}

fn sync_ui(ui: &BarUi, data: &Rc<RefCell<BarData>>) {
    let (entry, index, total, pinned) = {
        let d = data.borrow();
        let total = d.entries.len();
        let index = d
            .active_key
            .as_deref()
            .and_then(|k| d.entries.iter().position(|e| e.key == k))
            .unwrap_or_else(|| total.saturating_sub(1));
        (d.entries.get(index).cloned(), index, total, d.pinned)
    };

    if ui.pin_button.is_active() != pinned {
        ui.pin_button.set_active(pinned);
    }
    ui.pin_button.set_tooltip_text(Some(if pinned {
        "Unpin connection bar (auto-hide)"
    } else {
        "Pin connection bar"
    }));

    let Some(entry) = entry else { return };

    if entry.connected {
        ui.dot.add_css_class("connected");
    } else {
        ui.dot.remove_css_class("connected");
    }

    ui.title_label.set_label(&entry.title);
    let tooltip = match entry.hotkey_hint {
        Some(mod_name) => format!(
            "{}\n{mod_name} + Enter: full screen · + M: minimize · + G: grab · + D: disconnect",
            entry.title
        ),
        None => entry.title.clone(),
    };
    ui.title_label.set_tooltip_text(Some(&tooltip));
    ui.settings_button.set_tooltip_text(Some(if entry.is_file {
        "Default settings"
    } else {
        "Settings for this desktop"
    }));

    if total > 1 {
        ui.switch_button.set_label(&format!("{}/{total}", index + 1));
        ui.switch_button.set_tooltip_text(Some(&format!("Switch session ({total} active)")));
        ui.switch_button.set_visible(true);
    } else {
        ui.switch_button.set_visible(false);
    }
}

fn cancel_collapse(data: &Rc<RefCell<BarData>>) {
    if let Some(id) = data.borrow_mut().collapse_timer.take() {
        id.remove();
    }
}

fn expand(ui: &BarUi, data: &Rc<RefCell<BarData>>) {
    cancel_collapse(data);
    ui.stack.set_visible_child_name("bar");
    ui.window.set_default_size(1, 1);
}

fn collapse_now(ui: &BarUi, data: &Rc<RefCell<BarData>>) {
    if data.borrow().pinned {
        return;
    }
    ui.stack.set_visible_child_name("handle");
    ui.window.set_default_size(1, 1);
}

fn schedule_collapse(ui: &BarUi, data: &Rc<RefCell<BarData>>, delay_ms: u64) {
    cancel_collapse(data);
    if data.borrow().pinned {
        return;
    }
    let weak_ui = ui.downgrade();
    let data_weak = Rc::downgrade(data);
    let id = gtk::glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
        let (Some(ui), Some(data)) = (weak_ui.upgrade(), data_weak.upgrade()) else { return };
        data.borrow_mut().collapse_timer = None;
        if !data.borrow().pinned && !ui.motion.contains_pointer() {
            collapse_now(&ui, &data);
        }
    });
    data.borrow_mut().collapse_timer = Some(id);
}
