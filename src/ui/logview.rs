//! Session logs: the last lines of every resource's sessions, and a window
//! that shows them.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use adw::prelude::*;
use relm4::prelude::*;

/// Lines kept per resource.
pub const MAX_LINES: usize = 4000;

/// Redacted log lines per resource key, newest last.
#[derive(Debug, Default)]
pub struct LogStore {
    lines: HashMap<String, VecDeque<String>>,
    files: HashMap<String, PathBuf>,
}

impl LogStore {
    pub fn push(&mut self, key: &str, line: String) {
        let lines = self.lines.entry(key.to_owned()).or_default();
        if lines.len() == MAX_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Mark the start of a new session in the log.
    pub fn begin_session(&mut self, key: &str, title: &str) {
        let stamp = glib_time();
        self.push(key, format!("--- {title}: new connection {stamp} ---"));
    }

    pub fn set_file(&mut self, key: &str, file: PathBuf) {
        self.files.insert(key.to_owned(), file);
    }

    pub fn file(&self, key: &str) -> Option<&Path> {
        self.files.get(key).map(PathBuf::as_path)
    }

    pub fn lines(&self, key: &str) -> Vec<String> {
        self.lines.get(key).map(|l| l.iter().cloned().collect()).unwrap_or_default()
    }
}

fn glib_time() -> String {
    gtk::glib::DateTime::now_local()
        .ok()
        .and_then(|t| t.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The log window. One instance, reused for whichever resource is shown.
pub struct LogViewer {
    key: Option<String>,
    file: Option<PathBuf>,
}

#[derive(Debug)]
pub enum LogViewerMsg {
    Show { key: String, title: String, lines: Vec<String>, file: Option<PathBuf> },
    /// A new line for `key`; ignored unless that resource is shown.
    Append { key: String, line: String },
    /// The log file of the shown resource changed (a new session started).
    SetFile { key: String, file: PathBuf },
    OpenFile,
}

pub struct LogViewerWidgets {
    title: adw::WindowTitle,
    buffer: gtk::TextBuffer,
    view: gtk::TextView,
    open_button: gtk::Button,
    stack: gtk::Stack,
}

impl Component for LogViewer {
    type Init = gtk::Window;
    type Input = LogViewerMsg;
    type Output = ();
    type CommandOutput = ();
    type Root = adw::Window;
    type Widgets = LogViewerWidgets;

    fn init_root() -> Self::Root {
        adw::Window::builder().default_width(900).default_height(600).hide_on_close(true).build()
    }

    fn init(parent: Self::Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        root.set_transient_for(Some(&parent));
        let title = adw::WindowTitle::new("Connection log", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        let open_button = gtk::Button::with_label("Open log file");
        open_button.set_tooltip_text(Some("Open FreeRDP's complete log in the default text editor"));
        {
            let sender = sender.clone();
            open_button.connect_clicked(move |_| sender.input(LogViewerMsg::OpenFile));
        }
        header.pack_start(&open_button);

        let buffer = gtk::TextBuffer::new(None);
        let view = gtk::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_top_margin(8);
        view.set_bottom_margin(8);
        view.set_left_margin(12);
        view.set_right_margin(12);
        let scroller = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();

        let empty = adw::StatusPage::builder()
            .icon_name("text-x-generic-symbolic")
            .title("No log yet")
            .description("Connect to this desktop to see FreeRDP's messages here.")
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&scroller, Some("log"));
        stack.add_named(&empty, Some("empty"));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        root.set_content(Some(&toolbar));

        let widgets = LogViewerWidgets { title, buffer, view, open_button, stack };
        ComponentParts { model: LogViewer { key: None, file: None }, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        _sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            LogViewerMsg::Show { key, title, lines, file } => {
                widgets.title.set_subtitle(&title);
                root.set_title(Some(&format!("Connection log – {title}")));
                let mut text = lines.join("\n");
                if !text.is_empty() {
                    text.push('\n');
                }
                widgets.buffer.set_text(&text);
                self.key = Some(key);
                self.file = file;
                root.present();
                scroll_to_end(widgets);
            }
            LogViewerMsg::Append { key, line } => {
                if self.key.as_deref() != Some(key.as_str()) || !root.is_visible() {
                    return;
                }
                let mut end = widgets.buffer.end_iter();
                widgets.buffer.insert(&mut end, &line);
                let mut end = widgets.buffer.end_iter();
                widgets.buffer.insert(&mut end, "\n");
                // Keep the window as bounded as the store.
                let excess = widgets.buffer.line_count() - MAX_LINES as i32 - 1;
                if excess > 0 {
                    let mut start = widgets.buffer.start_iter();
                    if let Some(mut cut) = widgets.buffer.iter_at_line(excess) {
                        widgets.buffer.delete(&mut start, &mut cut);
                    }
                }
                scroll_to_end(widgets);
            }
            LogViewerMsg::SetFile { key, file } => {
                if self.key.as_deref() == Some(key.as_str()) {
                    self.file = Some(file);
                }
            }
            LogViewerMsg::OpenFile => {
                if let Some(file) = &self.file {
                    let launcher = gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(file)));
                    launcher.launch(Some(root), None::<&gtk::gio::Cancellable>, |result| {
                        if let Err(err) = result {
                            tracing::warn!("could not open the log file: {err}");
                        }
                    });
                }
            }
        }
        let has_text = widgets.buffer.char_count() > 0;
        widgets.stack.set_visible_child_name(if has_text { "log" } else { "empty" });
        widgets.open_button.set_sensitive(self.file.as_deref().is_some_and(Path::is_file));
    }
}

fn scroll_to_end(widgets: &LogViewerWidgets) {
    let mark = widgets.buffer.create_mark(None, &widgets.buffer.end_iter(), false);
    widgets.view.scroll_mark_onscreen(&mark);
    widgets.buffer.delete_mark(&mark);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_lines_per_resource() {
        let mut store = LogStore::default();
        for i in 0..MAX_LINES + 10 {
            store.push("a", format!("line {i}"));
        }
        store.push("b", "other".into());
        let lines = store.lines("a");
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], "line 10");
        assert_eq!(lines.last().unwrap(), &format!("line {}", MAX_LINES + 9));
        assert_eq!(store.lines("b"), ["other"]);
        assert!(store.lines("missing").is_empty());
    }

    #[test]
    fn remembers_log_files_and_marks_sessions() {
        let mut store = LogStore::default();
        assert_eq!(store.file("a"), None);
        store.set_file("a", "/tmp/x.log".into());
        assert_eq!(store.file("a"), Some(Path::new("/tmp/x.log")));
        store.begin_session("a", "Desk");
        assert!(store.lines("a")[0].starts_with("--- Desk: new connection"));
    }
}
