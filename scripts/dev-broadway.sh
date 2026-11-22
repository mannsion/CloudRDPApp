#!/bin/sh
# Run the app headlessly in GTK's Broadway backend, in demo mode, with a
# throw-away configuration. View it at http://127.0.0.1:8097/ in a browser.
# Nothing appears on the real desktop and the real settings are not touched.
#
#   scripts/dev-broadway.sh            demo mode
#   scripts/dev-broadway.sh --real     the real app (still isolated config)
# Extra environment is passed through, e.g. CLOUDRDPAPP_DEMO_FEED=fail.
set -eu
cd "$(dirname "$0")/.."

DISPLAY_NUM="${BROADWAY_DISPLAY_NUM:-7}"
PORT="${BROADWAY_PORT:-8097}"
MODE="--demo"
[ "${1:-}" = "--real" ] && MODE=""

cargo build
TMP="$(mktemp -d "${TMPDIR:-/tmp}/cloudrdpapp-broadway.XXXXXX")"
mkdir -p "$TMP/config" "$TMP/cache" "$TMP/state" "$TMP/data"

gtk4-broadwayd ":$DISPLAY_NUM" --port "$PORT" &
BROADWAYD=$!
trap 'kill "$BROADWAYD" 2>/dev/null || true; rm -rf "$TMP"' EXIT INT TERM
sleep 1

echo "Open http://127.0.0.1:$PORT/  (config in $TMP/config)"
XDG_CONFIG_HOME="$TMP/config" XDG_CACHE_HOME="$TMP/cache" \
XDG_STATE_HOME="$TMP/state" XDG_DATA_HOME="$TMP/data" \
GDK_BACKEND=broadway BROADWAY_DISPLAY=":$DISPLAY_NUM" \
    ./target/debug/cloudrdpapp $MODE
