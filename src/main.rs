use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // FreeRDP starts this executable as its sign-in helper.
    if cloudrdpapp::helper::bridge::is_helper_invocation(&args) {
        return cloudrdpapp::helper::bridge::run(&args);
    }
    // The desktop hands us a browser sign-in response (ms-appx-web:).
    if cloudrdpapp::helper::callback::is_callback_invocation(&args) {
        return cloudrdpapp::helper::callback::run(&args);
    }
    match args.get(1).map(String::as_str) {
        Some(cloudrdpapp::url_handler::ARG_REGISTER) => return register_url_handler(),
        Some(cloudrdpapp::url_handler::ARG_UNREGISTER) => {
            return match cloudrdpapp::url_handler::unregister() {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("cloudrdpapp: could not remove the sign-in handler: {err}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }
    cloudrdpapp::ui::run()
}

fn register_url_handler() -> ExitCode {
    use cloudrdpapp::url_handler::{self, Outcome};
    let result = std::env::current_exe()
        .map_err(|err| format!("cannot locate this program: {err}"))
        .and_then(|exe| url_handler::ensure_registered(&exe));
    match result {
        Ok(Outcome::Registered { replaced: Some(previous) }) => {
            println!("Registered as the handler for {}; it replaces {previous}.", url_handler::MIME_TYPE);
            ExitCode::SUCCESS
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cloudrdpapp: could not register the sign-in handler: {err}");
            ExitCode::FAILURE
        }
    }
}
