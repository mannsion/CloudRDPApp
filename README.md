# CloudRDPApp

**CloudRDPApp** is an unofficial, native Linux client for **Microsoft Azure Virtual Desktop (AVD)** and **Windows 365** cloud desktops and RemoteApps. Built with Rust, GTK 4, libadwaita, and Relm4, it signs in with your Microsoft Entra ID account, displays your assigned workspace resources, and launches connections using [FreeRDP](https://www.freerdp.com).

> **Disclaimer:** CloudRDPApp is an independent open-source project. It is **not** affiliated with, endorsed by, sponsored by, or supported by Microsoft Corporation.

## Features

- **Microsoft Entra ID sign-in:** Uses OAuth 2.0 with PKCE in your system web browser (with a manual paste-URL fallback if your browser cannot hand back `ms-appx-web:` links).
- **Workspace feed discovery & search:** Automatically fetches your assigned desktops and RemoteApps, caches feed icons, and filters resources as you type.
- **Silent connection SSO:** Answers FreeRDP's gateway and session-host token requests through a local OpenSSL token relay so no browser window pops up on every connection (falling back to interactive browser sign-in only when Entra policy requires it).
- **Per-desktop quality & display tuning:** Defaults to AVC 4:4:4 for crisp text, PipeWire/PulseAudio audio, and automatic Wayland/X11 scaling, with global defaults and per-desktop overrides.
- **Keyring integration:** Stores refresh tokens and optional NLA passwords in your Secret Service keyring (GNOME Keyring or KWallet/`ksecretd`), with a private file fallback when no keyring daemon is available.
- **Demo mode & `.rdp` files:** Includes an offline `--demo` mode for exploring the UI and supports opening standalone `.rdp` files directly.

## Dependencies

### Runtime and build requirements

- **FreeRDP 3.32 or later** (`sdl-freerdp3`, optionally `xfreerdp3`) — older versions lack the sign-in helper protocol used for silent connection SSO. The app displays a banner if the client is missing or too old.
- **GTK 4.18+** and **libadwaita 1.7+** (plus development headers when building from source)
- **OpenSSL** (system `libssl` and development headers for building)
- **Rust 1.95+** (edition 2024) and `pkg-config` for building from source
- **Secret Service keyring** (GNOME Keyring or KWallet/`ksecretd`) — optional; without one, saved credentials fall back to a user-readable private file (`0600`)

### Installing dependencies by distribution

**Arch Linux**
```sh
sudo pacman -S --needed rust gtk4 libadwaita openssl pkgconf freerdp
```

**Fedora**
```sh
sudo dnf install rust cargo gtk4-devel libadwaita-devel openssl-devel pkgconf-pkg-config freerdp
```

**Ubuntu / Debian** (current releases; use [rustup](https://rustup.rs/) if your distro packages Rust < 1.95)
```sh
sudo apt install cargo rustc libgtk-4-dev libadwaita-1-dev libssl-dev pkg-config
```
> **Note for Ubuntu / Debian:** Debian Stable and older Ubuntu LTS releases may ship FreeRDP older than 3.32 or GTK older than 4.18. If `sdl-freerdp3 --version` reports an older release, install FreeRDP 3.32+ from backports or [build FreeRDP from source](https://github.com/FreeRDP/FreeRDP).

**openSUSE Tumbleweed**
```sh
sudo zypper install rust cargo gtk4-devel libadwaita-devel libopenssl-devel pkgconf-pkg-config freerdp
```

## Quick Install

Clone the repository and run the installer script (no `sudo` required):

```sh
git clone https://github.com/mannsion/CloudRDPApp.git
cd CloudRDPApp
./scripts/install.sh
```

`scripts/install.sh` checks your Rust, GTK 4, libadwaita, OpenSSL, and FreeRDP versions, builds the release binary, installs it into `~/.local`, installs the `.desktop` entry and icons, and registers the `ms-appx-web:` URL handler for browser sign-in callbacks.

Useful installer options:

```sh
./scripts/install.sh --prefix "$HOME/opt/cloudrdpapp"   # install to a custom prefix
./scripts/install.sh --skip-checks                      # skip prerequisite version checks
./scripts/install.sh --uninstall                        # remove installed files and URL handler
```

### Building and packaging with `make`

You can also use the `Makefile` directly:

```sh
make build        # cargo build --release --locked
make install      # install binary, desktop file, icons, and ms-appx-web: handler into ~/.local
make uninstall    # remove installed files (keeps user config, cache, and logs)
make test         # run unit and integration tests
make check        # run cargo clippy (-D warnings) and cargo test
```

Distro packagers can pass `PREFIX` and `DESTDIR`:

```sh
make install PREFIX=/usr DESTDIR=/tmp/pkgroot
```

When `DESTDIR` is set, per-user URL handler registration and desktop database updates are skipped.

## Usage and CLI Options

Start **CloudRDPApp** from your application menu or run `~/.local/bin/cloudrdpapp`:

```sh
cloudrdpapp              # launch the application
cloudrdpapp --demo       # offline demo mode (fake account/feed; uses ~/.config/cloudrdpapp/demo-config.json)
cloudrdpapp --version    # print version and exit
cloudrdpapp --help       # show CLI flags
```

Useful environment variables:

- `CLOUDRDPAPP_LOG=debug` — print diagnostic logs to standard error.
- `CLOUDRDPAPP_DUMP_FEED=1` — log raw workspace feed responses for debugging.
- `CLOUDRDPAPP_DEMO=1` — same as `--demo`.
- `CLOUDRDPAPP_DEMO_FEED=empty|fail` — exercise empty or failed feed states in demo mode.
- `CLOUDRDPAPP_DEMO_FREERDP=missing|old` — exercise missing or outdated FreeRDP banners in demo mode.
- `scripts/dev-broadway.sh` — run demo mode headlessly in GTK's Broadway backend (view at `http://127.0.0.1:8097/`) using isolated temporary XDG directories.

## How Sign-In Works

1. **Signing in:** CloudRDPApp opens your default web browser to Microsoft's OAuth 2.0 + PKCE sign-in page using Microsoft's public Remote Desktop client ID (`a85cf173-4192-42f8-81fa-777a763e6e2c`) and web client feed header (`X-MS-User-Agent`) for interoperability with Azure Virtual Desktop and Windows 365 feeds. Because this is an unofficial client using those endpoints for interoperability, this workflow is not supported by Microsoft.
2. **Returning from the browser:** That public client ID only permits `ms-appx-web://Microsoft.AAD.BrokerPlugin/<client id>` and `.../nativeclient` redirects (not `http://localhost`). CloudRDPApp registers a hidden per-user handler (`~/.local/share/applications/io.github.mannsion.CloudRDPApp.AuthCallback.desktop` via `xdg-mime default`) so the browser can launch `cloudrdpapp --auth-callback <url>` and deliver the authorization code over a local Unix socket (`0600`). Any previously registered handler for `ms-appx-web:` (such as FreeRDP's `freerdp-aad-handler`) is logged before being replaced. If your browser cannot hand the link back automatically, choose **Paste the address instead** in the app and paste the final blank page's URL.
3. **Token storage:** Your refresh token is stored in your Secret Service keyring. Subsequent launches open straight to your cached feed while refreshing in the background.
4. **Silent connection SSO:** When connecting, CloudRDPApp downloads the resource's `.rdp` file and launches FreeRDP with a local OpenSSL token relay (`/azure:ad:127.0.0.1:<port>` + `/azure:auth-helper`). FreeRDP receives its gateway and session-host tokens silently from your saved session, so **no browser window appears when you connect** unless Entra ID Conditional Access requires fresh interaction.

For protocol details and internal design notes, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Settings That Matter for Quality

Open **Default Settings…** from the main menu, or **Settings for this desktop…** from any resource card's menu:

- **Graphics → Codec: AVC 4:4:4** (default) produces the sharpest text. Switch to **AVC 4:2:0** to reduce CPU usage if needed.
- **Frame rate** is governed by the remote host (typically 30 fps, or 60 fps if enabled by host policy); no client option forces a higher rate.
- **Graphics → Network: Detect automatically** (default) lets FreeRDP adapt to your connection quality.
- **Display → Full screen** uses your monitor's native resolution. **Scale: Automatic** follows your desktop scale (for example, 125% or 150% on HiDPI Wayland displays). Custom scaling applies to single-monitor sessions; spanning multiple monitors at fractional scales is a known limitation in FreeRDP.
- **Audio → Quality: High** and **Sound system: PulseAudio** (which works seamlessly over PipeWire). Increase the playback buffer if audio crackles.
- **Advanced → Shortcut modifier: Ctrl + Alt** (default, written to `~/.config/freerdp/sdl-freerdp.json`). FreeRDP's built-in default is Right Shift, which causes `Right Shift + D` to disconnect while typing.
- **Advanced → Log detail:** Debug and Trace levels can include sign-in tokens in local logs; use them only when diagnosing issues.

## Troubleshooting

- **Logs:** Each session writes FreeRDP output to `~/.local/state/cloudrdpapp/logs/` (keeping the 30 most recent logs). Click **View log** on a card for a redacted summary, or **Open Logs Folder** in the main menu.
- **Configuration & cache:** Settings live in `~/.config/cloudrdpapp/config.json`; cached feeds and icons live in `~/.cache/cloudrdpapp/`.
- **“FreeRDP … is not installed / is version …”:** Install FreeRDP 3.32+ (`sdl-freerdp3`), or configure a custom binary path under **Default Settings → Advanced → FreeRDP programs**.
- **“The gateway refused the connection. The sign-in token may have been rejected.”:** Retry once; if your tenant policy rejects silent token minting, switch **Advanced → Connection sign-in** to **Browser** for that desktop.
- **“This host requires password sign-in (NLA)”:** The host pool uses password authentication instead of Entra SSO. Set **Advanced → Session host sign-in** to **Password**; you will be prompted for your password (with an option to save it in the keyring).
- **“The remote computer rejected the sign-in.”:** If a saved password was used, CloudRDPApp removes it automatically so you are prompted again on the next attempt.
- **Entra sign-in errors (`AADSTS…`):** Search the `AADSTS` error code shown in the UI to check what device, MFA, or Conditional Access requirement your organization enforces.
- **`Right Shift + D` disconnected the session:** Ensure **Advanced → Shortcut modifier** is set to **Ctrl + Alt** and reconnect.

## Testing and Development

Run the full test suite and linter:

```sh
make check    # runs cargo clippy --all-targets -- -D warnings and cargo test
```

What the automated suite covers:

- **Unit tests (`cargo test --lib`):** OAuth requests, workspace feed XML parsing, `.rdp` rewriting, FreeRDP argument planning, the token relay, the helper bridge, session lifecycle, and GTK-free UI logic.
- **FreeRDP integration test (`tests/freerdp_integration.rs`):** Launches the installed `sdl-freerdp3` headlessly against a mock Entra endpoint and a fake TLS AVD gateway to verify `/args-from:fd:3`, `/azure:ad:`, `SSL_CERT_FILE` trust, the helper Unix socket, and bearer token delivery to the gateway. Set `CLOUDRDPAPP_IT_VERBOSE=1` to print the full FreeRDP debug log. (Automatically skips if `sdl-freerdp3` < 3.32 is not installed.)
- **Auth callback end-to-end test (`tests/auth_callback.rs`):** Invokes the built `cloudrdpapp --auth-callback <url>` binary against a waiting `Backend` and mock token endpoint to verify state validation, replay rejection, and code redemption.
- **Headless UI testing:** Run `scripts/dev-broadway.sh` to inspect the full GTK 4 / libadwaita UI in a browser without touching your real configuration.

## License and Trademarks

CloudRDPApp is licensed under the [MIT License](LICENSE) (`Copyright (c) 2026 mannsion`).

Microsoft, Windows, Windows 365, Azure, Azure Virtual Desktop, and Microsoft Entra are trademarks or registered trademarks of Microsoft Corporation. FreeRDP is a trademark of the FreeRDP project. All trademarks are the property of their respective owners.
