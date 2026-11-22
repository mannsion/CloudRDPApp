//! Small dialogs: password prompt, About, file chooser.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::gtk::gio;

use super::view::app_icon_name;
use crate::consts;

/// Ask for the password of a host that does not use single sign-on.
/// `on_answer` gets `Some((password, remember))`, or `None` when cancelled.
pub fn ask_password(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    storage: &str,
    on_answer: impl Fn(Option<(String, bool)>) + 'static,
) {
    let dialog = adw::AlertDialog::new(
        Some("Password Required"),
        Some(&format!("{title} signs in with a password instead of your Microsoft account. Enter the password for the remote computer.")),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("connect"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("connect", false);

    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let entry = adw::PasswordEntryRow::builder().title("Password").build();
    list.append(&entry);
    let remember = gtk::CheckButton::builder().label("Remember in the keyring").margin_top(12).build();
    remember.set_tooltip_text(Some(&format!("Stored in {storage}, for this account and this desktop only")));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&list);
    content.append(&remember);
    dialog.set_extra_child(Some(&content));

    {
        let dialog = dialog.clone();
        entry.connect_changed(move |entry| dialog.set_response_enabled("connect", !entry.text().is_empty()));
    }
    // Enter answers with the default response, "connect" (only once enabled).
    entry.set_activates_default(true);
    dialog.connect_response(None, move |_, response| {
        if response == "connect" {
            on_answer(Some((entry.text().to_string(), remember.is_active())));
        } else {
            on_answer(None);
        }
    });
    dialog.present(Some(parent));
}

pub fn about(parent: &impl IsA<gtk::Widget>) {
    let about = adw::AboutDialog::builder()
        .application_name(consts::APP_NAME)
        .application_icon(app_icon_name())
        .version(env!("CARGO_PKG_VERSION"))
        .license_type(gtk::License::MitX11)
        .website("https://github.com/mannsion/CloudRDPApp")
        .issue_url("https://github.com/mannsion/CloudRDPApp/issues")
        // Shown on the first page, where everyone sees it.
        .developer_name("mannsion · Unofficial, not affiliated with Microsoft")
        .comments(
            "An unofficial Linux client for Azure Virtual Desktop and Windows 365. It signs in with Microsoft Entra ID, \
             lists your desktops and apps, and connects with FreeRDP.\n\n\
             Not affiliated with, endorsed by or supported by Microsoft.",
        )
        .build();
    about.add_acknowledgement_section(Some("Connections by"), &["FreeRDP https://www.freerdp.com"]);
    about.add_legal_section(
        "Trademarks",
        None,
        gtk::License::Unknown,
        Some("Microsoft, Windows, Windows 365, Azure, and Microsoft Entra are trademarks of the Microsoft group of companies. They are used here only to describe what this program connects to."),
    );
    about.present(Some(parent));
}

/// Let the user pick a `.rdp` file.
pub fn choose_rdp_file(parent: &impl IsA<gtk::Window>, on_file: impl FnOnce(PathBuf) + 'static) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Remote Desktop files"));
    filter.add_pattern("*.rdp");
    filter.add_pattern("*.RDP");
    filter.add_mime_type("application/x-rdp");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder().title("Open a Remote Desktop File").filters(&filters).default_filter(&filter).build();
    dialog.open(Some(parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            on_file(path);
        }
    });
}
