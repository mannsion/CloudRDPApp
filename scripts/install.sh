#!/usr/bin/env bash
# Build and install CloudRDPApp from source for the current user.
set -euo pipefail

APP_ID="io.github.mannsion.CloudRDPApp"
BIN="cloudrdpapp"
PREFIX="${HOME}/.local"
UNINSTALL=0
SKIP_CHECKS=0

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [OPTIONS]

Build and install CloudRDPApp (binary, desktop entry, icons, and ms-appx-web:
URL handler) into PREFIX (default: ~/.local).

Options:
  --prefix <dir>   Install prefix (default: $HOME/.local)
  --uninstall      Remove installed files and unregister the URL handler
  --skip-checks    Skip the prerequisite version checks before building
  -h, --help       Show this help and exit
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || { echo "Error: --prefix requires an argument." >&2; exit 2; }
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --skip-checks)
            SKIP_CHECKS=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Error: unknown option '$1'" >&2
            usage >&2
            exit 2
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

BINDIR="${PREFIX}/bin"
DATADIR="${PREFIX}/share"

version_ge() {
    local have="$1" need="$2"
    local IFS=.
    local -a a b
    read -r -a a <<< "${have%%[-+]*}"
    read -r -a b <<< "${need%%[-+]*}"
    local i len=${#b[@]}
    if (( ${#a[@]} > len )); then
        len=${#a[@]}
    fi
    for (( i = 0; i < len; i++ )); do
        local av="${a[i]:-0}" bv="${b[i]:-0}"
        av="${av%%[^0-9]*}"; av="${av:-0}"
        bv="${bv%%[^0-9]*}"; bv="${bv:-0}"
        if (( 10#$av > 10#$bv )); then
            return 0
        fi
        if (( 10#$av < 10#$bv )); then
            return 1
        fi
    done
    return 0
}

print_distro_hints() {
    cat >&2 <<'EOF'

Install build dependencies for your distribution:
  Arch Linux:
    sudo pacman -S --needed rust gtk4 libadwaita openssl pkgconf freerdp
  Fedora:
    sudo dnf install rust cargo gtk4-devel libadwaita-devel openssl-devel pkgconf-pkg-config freerdp
  Ubuntu / Debian (25.04+ / Sid, or newer):
    sudo apt install cargo rustc libgtk-4-dev libadwaita-1-dev libssl-dev pkg-config
    (Note: Ubuntu/Debian may ship an older Rust or FreeRDP; use rustup for Rust 1.95+
     and build FreeRDP 3.32+ from source if your distro repository is older.)
  openSUSE Tumbleweed:
    sudo zypper install rust cargo gtk4-devel libadwaita-devel libopenssl-devel pkgconf-pkg-config freerdp
EOF
}

check_prerequisites() {
    local -a errors=()

    if ! command -v cargo >/dev/null 2>&1; then
        errors+=("cargo not found (Rust 1.95+ toolchain required)")
    fi

    if ! command -v rustc >/dev/null 2>&1; then
        errors+=("rustc not found (Rust 1.95+ required)")
    else
        local rustc_ver
        rustc_ver="$(rustc --version | awk '{print $2}')"
        if ! version_ge "$rustc_ver" "1.95"; then
            errors+=("rustc $rustc_ver is too old (Rust 1.95+ required)")
        fi
    fi

    if ! command -v pkg-config >/dev/null 2>&1; then
        errors+=("pkg-config not found")
    else
        if ! pkg-config --exists gtk4; then
            errors+=("gtk4 development files not found (GTK 4.18+ required)")
        elif ! pkg-config --atleast-version=4.18 gtk4; then
            errors+=("gtk4 $(pkg-config --modversion gtk4) is too old (GTK 4.18+ required)")
        fi

        if ! pkg-config --exists libadwaita-1; then
            errors+=("libadwaita-1 development files not found (libadwaita 1.7+ required)")
        elif ! pkg-config --atleast-version=1.7 libadwaita-1; then
            errors+=("libadwaita-1 $(pkg-config --modversion libadwaita-1) is too old (libadwaita 1.7+ required)")
        fi

        if ! pkg-config --exists openssl; then
            errors+=("openssl development files not found (libssl / openssl-devel required)")
        fi
    fi

    if (( ${#errors[@]} > 0 )); then
        echo "Missing or outdated build prerequisites:" >&2
        for err in "${errors[@]}"; do
            echo "  - $err" >&2
        done
        print_distro_hints
        exit 1
    fi

    # Runtime dependency check: warn instead of failing the build.
    if ! command -v sdl-freerdp3 >/dev/null 2>&1; then
        echo "Warning: sdl-freerdp3 not found in PATH." >&2
        echo "  CloudRDPApp will build, but connecting requires FreeRDP 3.32+ (sdl-freerdp3)." >&2
        echo "  Install your distro's freerdp package or build FreeRDP 3.32+ from source." >&2
    else
        local freerdp_out freerdp_ver=""
        freerdp_out="$(sdl-freerdp3 --version 2>/dev/null || true)"
        if [[ "$freerdp_out" =~ ([0-9]+\.[0-9]+(\.[0-9]+)?) ]]; then
            freerdp_ver="${BASH_REMATCH[1]}"
        fi
        if [[ -n "$freerdp_ver" ]] && ! version_ge "$freerdp_ver" "3.32"; then
            echo "Warning: sdl-freerdp3 $freerdp_ver is older than 3.32." >&2
            echo "  Silent connection sign-in requires FreeRDP 3.32 or later." >&2
        fi
    fi
}

do_uninstall() {
    if [[ -x "${BINDIR}/${BIN}" ]]; then
        "${BINDIR}/${BIN}" --unregister-url-handler || true
    fi
    rm -f "${BINDIR}/${BIN}"
    rm -f "${DATADIR}/applications/${APP_ID}.desktop"
    rm -f "${DATADIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
    rm -f "${DATADIR}/icons/hicolor/symbolic/apps/${APP_ID}-symbolic.svg"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "${DATADIR}/applications" 2>/dev/null || true
    fi
    touch -c "${DATADIR}/icons/hicolor" 2>/dev/null || true

    echo "Uninstalled CloudRDPApp from ${PREFIX}."
    echo "User settings (~/.config/cloudrdpapp), cache (~/.cache/cloudrdpapp),"
    echo "and logs (~/.local/state/cloudrdpapp/logs) are kept;"
    echo "sign out in the app first to remove the saved sign-in from your keyring."
}

if (( UNINSTALL )); then
    do_uninstall
    exit 0
fi

if (( ! SKIP_CHECKS )); then
    check_prerequisites
fi

echo "Building CloudRDPApp (release)..."
cargo build --release --locked

echo "Installing to ${PREFIX}..."
install -Dm755 "target/release/${BIN}" "${BINDIR}/${BIN}"
install -Dm644 "data/${APP_ID}.desktop" "${DATADIR}/applications/${APP_ID}.desktop"
install -Dm644 "data/icons/hicolor/scalable/apps/${APP_ID}.svg" \
    "${DATADIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
install -Dm644 "data/icons/hicolor/symbolic/apps/${APP_ID}-symbolic.svg" \
    "${DATADIR}/icons/hicolor/symbolic/apps/${APP_ID}-symbolic.svg"

sed -i "s|^Exec=.*|Exec=${BINDIR}/${BIN}|" "${DATADIR}/applications/${APP_ID}.desktop"

"${BINDIR}/${BIN}" --register-url-handler
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "${DATADIR}/applications" 2>/dev/null || true
fi
touch -c "${DATADIR}/icons/hicolor" 2>/dev/null || true

echo "Installed CloudRDPApp to ${BINDIR}/${BIN}."
echo "Start it from your application menu or run: ${BINDIR}/${BIN}"
