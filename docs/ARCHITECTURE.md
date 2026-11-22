# CloudRDPApp architecture and working notes

A native Linux client (Rust, Relm4 / GTK 4 / libadwaita) for Azure Virtual Desktop
and Windows 365: sign in with Microsoft Entra ID, list the desktops/apps from the
workspace feed (what `https://windows.cloud.microsoft` shows), connect with
FreeRDP, with a settings GUI tuned for resolution, frame rate, and sound.

## Flow

1. **Sign-in** — OAuth 2.0 auth-code + PKCE in the *system browser* on the
   broker redirect `ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>`.
   Public client id `a85cf173-4192-42f8-81fa-777a763e6e2c`, scope
   `https://www.wvd.microsoft.com/.default offline_access openid profile`,
   authority `login.microsoftonline.com/organizations`. Fallback: the
   `…/common/oauth2/nativeclient` redirect with the user pasting the final URL.
   The refresh token lives in the Secret Service keyring (`oo7`), with a private
   file fallback when no keyring daemon is available.

   **Registered redirects of this client:** only
   `ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>` (FreeRDP's default
   `FreeRDP_GatewayAvdAccessTokenFormat`) and
   `https://login.microsoftonline.com/common/oauth2/nativeclient`. A loopback
   redirect is **not** registered: `http://localhost:<port>/` fails with
   `AADSTS50011`.

   Return path (`auth::callback`, `helper::callback`, `url_handler`): the
   browser hands the `ms-appx-web:` URL to the desktop's handler for
   `x-scheme-handler/ms-appx-web`, a hidden per-user desktop entry
   `~/.local/share/applications/io.github.mannsion.CloudRDPApp.AuthCallback.desktop`
   with `Exec=<absolute exe> --auth-callback %u`, made the default with
   `xdg-mime default`. The app (re)writes it right before every browser
   sign-in when it does not point at the running executable (so `cargo run`
   works); `make install` and `scripts/install.sh` run
   `cloudrdpapp --register-url-handler`, and `uninstall` runs
   `--unregister-url-handler` (removes the entry and the `mimeapps.list`
   associations). A different previous handler is logged (`warn`) before it is
   replaced. `cloudrdpapp --auth-callback <url>` runs before any GUI
   initialisation: it checks the URL (prefix
   `ms-appx-web://microsoft.aad.brokerplugin/` ignoring ASCII case, at most
   32 KiB, no control characters) and sends `{"auth_callback": url}` to every
   `helper-*.sock` in `paths::runtime_dir()` (newest first) until one accepts.
   The app's `CallbackHub` hands it to the sign-in waiting for its `state`
   (random, single use, removed on cancel/timeout); that sign-in redeems the
   code with the same redirect URI. FreeRDP's upstream
   `client/common/xdg-aad-helper` works the same way (it routes by a prefix it
   adds to `state`).
2. **Feed** — `GET https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery`
   (bearer token + approved `X-MS-User-Agent`) → `TenantFeedURL` entries → each
   feed is a `ResourceCollection` of `Resource`s with icons and a
   `ResourceFile URL` → that URL returns the `.rdp` file.
3. **Connect** — rewrite the downloaded `.rdp` where needed, launch
   `sdl-freerdp3` (or `xfreerdp3`) with `/args-from:fd:3`, and answer its
   connection-time token requests silently (below).

## Facts verified against FreeRDP 3.33.1 source

FreeRDP source reference:
`git clone --depth 1 --branch 3.33.1 https://github.com/FreeRDP/FreeRDP`.

- **Two tokens per connection.** `libfreerdp/core/gateway/arm.c` asks for the
  gateway token (`ACCESS_TOKEN_TYPE_AVD`, scope
  `https://www.wvd.microsoft.com/.default openid profile offline_access`) after
  TLS-connecting to the gateway. With `/sec:aad`, `libfreerdp/core/aad.c` asks
  for a session-host token (`ACCESS_TOKEN_TYPE_AAD`, scope
  `ms-device-service://termsrv.wvd.microsoft.com/name/<host>/user_impersonation`)
  with `req_cnf` binding it to a key FreeRDP generates.
- **Helper protocol** (`client/common/aad-helper-protocol.md`, `aad_helper.c`,
  3.32+). All clients use `client_failsafe_get_access_token`: helper first,
  terminal paste as fallback. FreeRDP spawns
  `"<helper>" --cmdInFd=P<hexfd> --cmdOutFd=P<hexfd>` and speaks
  newline-delimited JSON-RPC: `hello`, `navigate{title,url,redirect_uri,timeout_ms}`
  → `{result:{status:"ok",redirect_url}}` or `{error:{code,message}}`,
  `shutdown`, notification `exit`. The helper is chosen with
  `/azure:auth-helper:<path>`. FreeRDP then extracts `code=` and checks
  `state=` in `redirect_url` (`client/common/oauth2.c`) and redeems the code
  itself at the token endpoint. Distro packages often ship no helper binaries,
  but the protocol is compiled in.
- **Authority is configurable.** `/azure:ad:<host[:port]>` makes FreeRDP fetch
  `https://<ad>/common/v2.0/.well-known/openid-configuration` and use its
  `authorization_endpoint`/`token_endpoint`. Its HTTPS client
  (`libfreerdp/utils/http.c`) requires `https://`, HTTP/1.1 and a
  `Content-Length` response header (no chunked), verifies the chain against
  OpenSSL's default store (so `SSL_CERT_FILE` works) and does not check the host
  name. It also posts `grant_type=srv_challenge` to the token endpoint for a nonce.
- **`/args-from:`** reads one argument per line until a blank line/EOF; the
  first line may be the `.rdp` path (`option_is_rdp_file`). It `fclose`s the
  stream afterwards (`args_from_fp`). With `/args-from:stdin` that closes
  stdin, and WinPR's `CreateProcess` then refuses to spawn the auth helper
  (the helper inherits the std handles; `aad-auth-helper: failed to spawn`).
  So the app passes the arguments on a private pipe as `/args-from:fd:3` and
  gives FreeRDP `/dev/null` as stdin.
- **SNI with port.** `freerdp_http_request` passes the URL authority
  (`127.0.0.1:<port>`) to `SSL_set_tlsext_host_name`. rustls servers reject
  that ClientHello (`illegal_parameter`), so the relay's TLS server is OpenSSL
  (`openssl`/`tokio-openssl` crates, system libssl).
- **ARM needs** `loadbalanceinfo` and `remoteapplicationprogram` in the `.rdp`
  (`arm_resolve_endpoint`); never strip them. `.rdp` display properties such as
  `use multimon:i:1` override the command line, so the file is rewritten.
- **Graphics.** No `/gfx` argument = GFX with AVC444+AVC420+RFX; naming codecs
  enables exactly those. No `/network` = autodetect. Typical distro FreeRDP
  builds enable `WITH_GFX_H264=ON` via FFmpeg (software decode,
  `WITH_FFMPEG_HWACCEL=OFF`) and pulse/alsa/oss/sndio audio backends.
- **Display & Wayland fractional scaling.** The SDL client detects monitor
  fractional scaling and native resolution automatically on both Wayland and
  X11 drivers. `/scale-desktop:N` overrides scale only when explicit override
  mode is enabled (single monitor). Multi-monitor sessions at fractional scales
  are known to fail in FreeRDP (overlap/gap checks).
- **SDL hotkeys** default to Right Shift + key (D disconnects!). The modifier is
  only configurable in `~/.config/freerdp/sdl-freerdp.json` (`SDL_KeyModMask`,
  all listed modifiers must be held, `KMOD_NONE` disables).
- **Logging.** Console WLog output is block-buffered on pipes; with
  `WLOG_APPENDER=file` + `WLOG_FILEAPPENDER_OUTPUT_FILE_PATH/NAME` every message
  is flushed. "Local framebuffer format" marks an established session. Exit
  codes: `rdp::freerdp::describe_exit`.

## Connection sign-in modes (`ConnectAuth::Silent` vs `ConnectAuth::Browser`)

- **Silent route (`ConnectAuth::Silent`, default):** `src/relay.rs` runs a local
  Entra "authority" on `127.0.0.1` with an ephemeral self-signed certificate,
  served by OpenSSL (see SNI above). FreeRDP is started with
  `/azure:ad:127.0.0.1:<port>,auth-helper:<this exe>` and
  `SSL_CERT_FILE=<system roots + relay cert>`. When FreeRDP navigates, the
  helper bridge (`src/helper/bridge.rs`, this same binary) forwards the request
  over a Unix socket to the app (`src/helper/server.rs`); the app issues a
  one-time code (`TokenRelay::issue_code(scope)`) and answers
  `<redirect_uri>?code=<code>&state=<state from the url>`. FreeRDP redeems the
  code at the relay, which performs the equivalent `refresh_token` grant against
  real Entra ID (forwarding `req_cnf`) via `AuthManager::token_response` and
  returns the result. No browser, no token on any command line. If Entra
  requires interactive authentication (e.g., Conditional Access claims), the
  relay falls back to opening the browser with a `login_hint` and `claims`
  (enforcing the same account).
- **Browser route (`ConnectAuth::Browser`, per-desktop fallback):** configured via
  `rdp::args::TokenRoute::Browser`, this leaves FreeRDP pointing at real Entra
  with only `/azure:auth-helper:<exe>`. With a helper
  (`UseCommonStdioCallbacks` off), FreeRDP's `get_redirect_uri` uses the
  `avd-token` format for both the gateway and session-host requests (defaulting
  to the broker redirect). The navigate handler registers FreeRDP's `state`
  with `CallbackHub`, opens the authorize URL in the browser, and returns the
  `--auth-callback` URL unchanged; FreeRDP checks `state` and redeems the code
  itself. (Without a helper, FreeRDP's terminal fallback uses `avd-access`,
  default `nativeclient`, for pasting.)

## Modules (all with unit tests; `cargo test --lib`)

| Module | Role |
|---|---|
| `consts`, `util`, `paths` | protocol constants, redaction/encoding, XDG paths |
| `auth::{oauth,account,callback,store,manager}` | Entra requests, ID token, broker callback validation and `CallbackHub`, keyring, `AuthManager` |
| `feed::{model,parse,client}` | feed model, XML parsing, HTTP with trusted-host check |
| `config` | `ConnectionSettings` (display/graphics/audio/devices/advanced), `Config` file |
| `rdp::file` | `.rdp` parse/edit |
| `rdp::args` | settings → FreeRDP arguments + `.rdp` edits (`LaunchPlan`) |
| `rdp::freerdp` | find binaries, version, exit codes, log interpretation |
| `rdp::sdl_config` | SDL hotkey modifier file |
| `rdp::session` | spawn FreeRDP, tail its log, events, stop |
| `relay` | token relay described above |
| `helper::{bridge,server,callback}` | FreeRDP helper process, its app-side socket (also takes sign-in callbacks), `--auth-callback` forwarder |
| `url_handler` | per-user `ms-appx-web:` handler desktop entry and `xdg-mime` default |
| `backend` | the engine the GUI calls: account, feed cache, icons, connect, navigate handlers |
| `ui::app` | main Relm4 component: start-up, sign-in states, feed sections, sessions, password retry, settings saves |
| `ui::view` | main window widget tree (header, banner, sign-in page, grid/state pages) and `AppWidgets::sync` |
| `ui::card` | one resource card, a factory component in a `gtk::FlowBox` per workspace |
| `ui::settings` | `adw::PreferencesDialog` for defaults and per-resource overrides, built from row helpers |
| `ui::logview` | `LogStore` (last 4000 lines per resource) and the log window |
| `ui::dialogs` | password prompt (`adw::AlertDialog`), About, `.rdp` file chooser |
| `ui::service` | `Service`: the real `Backend` or the demo behind one interface; `SessionHandle` |
| `ui::demo` | `--demo`: fake account/feed, scripted sessions, no network or keyring |
| `ui::logic` | GTK-free view decisions (search, card state after exit, error texts, banner) |

`tests/freerdp_integration.rs` runs the installed `sdl-freerdp3` (skips if
missing or < 3.32) against a mock Entra token endpoint and a fake TLS AVD
gateway, headless (`SDL_VIDEODRIVER=dummy`, `-sdl-connection-dialog`,
isolated `XDG_CONFIG_HOME`). It proves `/args-from:fd:3`, `/azure:ad:`,
trust via `SSL_CERT_FILE`, the helper bridge + socket, the one-time code and
redirect shape, and the relay's token response: the gateway receives
`Authorization: Bearer <minted token>` on `POST /api/arm/v2/connections`.
It cannot reach the session-host leg (`/sec:aad`, `req_cnf`, `srv_challenge`).

`tests/auth_callback.rs` runs the built executable as
`cloudrdpapp --auth-callback <url>` (with `XDG_RUNTIME_DIR` in a temp dir)
against a `Backend` whose sign-in waits, with a mock token endpoint: foreign
and wrong-state URLs are refused, the right one completes the sign-in with
`redirect_uri` = broker redirect, a replay is refused.
`CLOUDRDPAPP_IT_VERBOSE=1` prints FreeRDP's debug log.

## GUI notes (`src/ui/`)

- Relm4 0.11 runtime: `RELM_THREADS` is set to 4; `Backend::start` runs as a
  component command, so the relay and helper tasks live on Relm4's Tokio
  runtime. The backend callbacks send `AppMsg::OpenUrl`/`Status` to the main
  thread; session events arrive as `Cmd::Session { key, generation, event }`
  (a generation counter drops events of a replaced session).
- On close every session is stopped; `ui::run` then waits up to 5 s for the
  FreeRDP processes to report their exit (`LIVE_SESSIONS`), so none outlives
  the app.
- `PasswordRequired` → remembered password (keyring key `<account id>/<resource key>`,
  `KIND_PASSWORD`) → else `adw::AlertDialog`. A remembered password that the
  host rejects (exit 132/134/154/159 or a logon-failure log line) is deleted.
- Card failure text: last `explain_log_line` match, else `describe_exit`,
  else the raw code (`ui::logic::state_after_exit`).
- Settings choice rows are `adw::ActionRow` + `gtk::DropDown`, not
  `adw::ComboRow`: next to the long explanatory subtitles the combo row
  ellipsized its value. The dialog is 760 px wide for the same reason.
- Demo mode stores settings in `demo-config.json` and never writes the
  account; its logs go to `$TMPDIR/cloudrdpapp-demo-<uid>/`.
- Headless check: `scripts/dev-broadway.sh` (isolated XDG dirs except
  `XDG_RUNTIME_DIR`, where broadwayd's socket lives). Known GTK Broadway quirks
  when testing headlessly: the first click after a change often only hovers;
  synthetic text typing does not arrive (key presses do); some symbolic icons
  (drop-down arrows, toggled buttons) stop drawing until reload; and desktop
  portal actions ("Open .rdp File…" or "Open Logs Folder") may open on the
  real desktop session.

Packaging: `data/` (desktop file, icons), `Makefile` (`build`, `install`,
`uninstall`, `test`, `check`, supporting `PREFIX` and `DESTDIR`),
`scripts/install.sh`, `LICENSE`, `README.md`.

## Backend API (`src/backend.rs`)

The GUI creates one `Backend` (cheap to clone, `Arc` inside) on a Tokio
runtime and calls it from there. Callbacks: `OpenUrl = Arc<dyn Fn(&str) + Send + Sync>`
(open in the browser), `StatusSink = Arc<dyn Fn(String) + Send + Sync>` (status line).
Errors are `BackendError`; `Display` is a full sentence for the user.

| Call | What it does |
|---|---|
| `Backend::start(open_url, status, sdl_path: &str, x11_path: &str) -> Result<Backend, String>` | keyring, relay, helper socket in `paths::runtime_dir()`, detect clients (empty path = `PATH`), prune old logs |
| `Backend::with_parts(BackendParts) -> Result<Backend, String>` | same from injected `AuthManager`, `FeedClient`, dirs, `Clients`, extra child env, `url_handler_exe` (`None` = never touch the desktop's handlers; tests) |
| `clients() -> Clients` / `redetect_clients(sdl, x11).await -> Clients` | detected `sdl`/`x11` `rdp::freerdp::Client`s (path, version) |
| `account().await -> Option<Account>` | signed-in account |
| `restore_account(&Account).await -> bool` | load the saved refresh token (false = sign in again) |
| `sign_in().await -> Result<Account, BackendError>` | browser sign-in on the broker redirect (registers the `ms-appx-web:` handler first), returned by `--auth-callback`, 5 min; cancel by dropping the future |
| `start_manual_sign_in() -> PendingLogin` | fallback: opens the URL; the user pastes the final address |
| `finish_manual_sign_in(&PendingLogin, pasted: &str).await -> Result<Account, BackendError>` | completes the fallback |
| `sign_out().await` | forget account, keyring entry and cached feed |
| `cached_feed(&Account) -> Option<Feed>` | last feed from `cache_dir()/feeds/`, for an instant first screen (sync) |
| `fetch_feed().await -> Result<Feed, BackendError>` | download (401 → refresh token, retry once) and cache |
| `icon(&Resource).await -> Option<PathBuf>` | `cache_dir()/icons/<key>.<ext>`, downloaded on demand; `None` on any failure |
| `connect(&Resource, &ConnectionSettings, password: Option<&str>, local_scale_percent: Option<u32>, events: EventSink).await -> Result<Session, BackendError>` | download `.rdp` (401 retry), check client/version, plan, start FreeRDP |
| `connect_rdp(rdp_text, title, &ConnectionSettings, password, scale, events).await -> Result<Session, BackendError>` | same for an opened `.rdp` file |
| `Session::stop()`, `Session::log_file()`, `session.title`, `session.client` | disconnect (also on drop), FreeRDP log path |

`BackendError::PasswordRequired` means the host signs in with a password
(`!uses_entra_sso`) and none was given: prompt and call `connect` again.
`ClientMissing(kind)` / `ClientTooOld { kind, version }` mean FreeRDP must be
installed or updated. `events` receives `SessionEvent::{Log, Connected, Exited}`
on runtime threads; the helper registration is dropped on `Exited`.

While FreeRDP connects, its navigate requests are answered without the GUI:
relay route (`ConnectAuth::Silent`) mints the token silently, and only if
Entra demands interaction opens the browser (status: "Sign in again in your
browser…", login hint + claims, same account enforced, 170 s); browser route
(`ConnectAuth::Browser`) opens FreeRDP's URL and returns the `--auth-callback`
URL carrying FreeRDP's `state`.
Only gateway and session-host scopes are served.
