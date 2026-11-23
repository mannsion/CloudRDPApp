# CloudRDPApp

An unofficial, native Linux client for **Microsoft Azure Virtual Desktop** and **Windows 365** cloud desktops and RemoteApps.

Built with Rust, GTK 4 / libadwaita, and powered by [FreeRDP](https://www.freerdp.com).

> Independent open-source project — not affiliated with, endorsed by, or supported by Microsoft.

## Features

- **Native Linux UI** — GTK 4 / libadwaita workspace launcher with live search and Wayland HiDPI support.
- **Browser SSO sign-in** — Sign in once with Microsoft Entra ID; credentials are stored in your desktop keyring (GNOME Keyring / KWallet).
- **Silent connections** — Connects to your desktops and RemoteApps without popping up a browser login window every time.
- **Per-desktop settings** — Tune display mode, scaling, AVC 4:4:4 graphics, audio, microphone, clipboard, and folder sharing globally or per desktop.
- **Standalone `.rdp` files** — Open `.rdp` files directly from the app menu.

## Requirements

- **FreeRDP 3.32+** (`sdl-freerdp3`, optionally `xfreerdp3`)
- **GTK 4.18+** and **libadwaita 1.7+**
- **OpenSSL** (`libssl`)
- **Rust 1.95+** and `pkg-config` (to build from source)
- A Secret Service keyring (GNOME Keyring or KWallet) — optional

### Install Dependencies

**Arch Linux**
```sh
sudo pacman -S --needed rust gtk4 libadwaita openssl pkgconf freerdp
```

**Fedora**
```sh
sudo dnf install rust cargo gtk4-devel libadwaita-devel openssl-devel pkgconf-pkg-config freerdp
```

**Ubuntu / Debian**
```sh
sudo apt install cargo rustc libgtk-4-dev libadwaita-1-dev libssl-dev pkg-config
```
*(Use [rustup](https://rustup.rs/) if your distro ships Rust < 1.95, and install or [build FreeRDP 3.32+](https://github.com/FreeRDP/FreeRDP) if your distro package is older.)*

**openSUSE Tumbleweed**
```sh
sudo zypper install rust cargo gtk4-devel libadwaita-devel libopenssl-devel pkgconf-pkg-config freerdp
```

## Install

```sh
git clone https://github.com/mannsion/CloudRDPApp.git
cd CloudRDPApp
./scripts/install.sh
```

Installs to `~/.local` (no `sudo` required), adds the app menu entry, and registers the browser sign-in redirect handler.

- Custom prefix: `./scripts/install.sh --prefix "$HOME/opt/cloudrdpapp"`
- Uninstall: `./scripts/install.sh --uninstall`
- Using `make`: `make build`, `make install`, `make uninstall`, `make test`

## Usage

Launch **CloudRDPApp** from your application menu, or run `cloudrdpapp`:

- `cloudrdpapp --demo` — try the UI offline with sample desktops (no account or FreeRDP needed)
- `CLOUDRDPAPP_LOG=debug cloudrdpapp` — print diagnostic logs to stderr

When signing in, your browser will ask to open **CloudRDPApp** — click **Allow**. If your browser doesn't hand the link back automatically, choose **Paste the address instead**.

Session logs are kept in `~/.local/state/cloudrdpapp/logs/` and settings in `~/.config/cloudrdpapp/config.json`. Technical and protocol details live in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

Licensed under the [MIT License](LICENSE).

All product names and trademarks belong to their respective owners.
