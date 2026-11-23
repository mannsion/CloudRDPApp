//! The graphical interface: Relm4 on GTK 4 and libadwaita.
//!
//! | File | Role |
//! |---|---|
//! | `app` | the main component: model, messages, start-up, sign-in, sessions |
//! | `view` | the main window's widget tree and how it follows the model |
//! | `card` | one desktop or app in the grid (factory component) |
//! | `settings` | the settings dialog for defaults and per-resource overrides |
//! | `logview` | per-resource log store and the log window |
//! | `dialogs` | password prompt, About, file chooser |
//! | `service` | real backend or demo behind one interface |
//! | `demo` | offline demo mode |
//! | `layer_shell` | safe wrapper for `gtk4-layer-shell` Wayland overlays |
//! | `session_bar` | floating top-center connection bar for active sessions |
//! | `logic` | GTK-free view decisions, unit tested |

mod app;
mod card;
mod demo;
mod dialogs;
mod layer_shell;
mod logic;
mod logview;
mod service;
mod session_bar;
mod settings;
mod view;

use std::process::ExitCode;

use relm4::RelmApp;

use crate::consts;

pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("{} {}", consts::APP_NAME, env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: cloudrdpapp [--demo] [--version]\n\n  --demo                    run with a fake account and feed (no network, no keyring)\n  --version                 print the version and exit\n  --auth-callback URL       hand a browser sign-in response (ms-appx-web:) to the running app\n  --register-url-handler    make this program the ms-appx-web: handler for the current user\n  --unregister-url-handler  remove that registration\n\nEnvironment: CLOUDRDPAPP_LOG=debug for diagnostics on standard error."
        );
        return ExitCode::SUCCESS;
    }
    init_logging();
    crate::install_crypto_provider();
    let demo = demo::requested(&args);

    let _ = relm4::RELM_THREADS.set(4);
    let app = RelmApp::new(consts::APP_ID).with_args(vec![]);
    if demo {
        // A demo must never hand over to (or block) a real running instance.
        app.allow_multiple_instances(true);
    }
    gtk::glib::set_application_name(consts::APP_NAME);
    gtk::Window::set_default_icon_name(consts::APP_ID);
    app.run::<app::App>(app::AppInit { demo });
    app::wait_for_sessions(std::time::Duration::from_secs(5));
    ExitCode::SUCCESS
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CLOUDRDPAPP_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
