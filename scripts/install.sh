#!/usr/bin/env bash
# Builds Clawd and installs it for the current user: the binary, its icon and
# an app-launcher entry (so rofi, app menus and docks can start it).
#
#   scripts/install.sh            # into ~/.local
#   PREFIX=/some/where scripts/install.sh
set -euo pipefail
cd "$(dirname "$0")/.."

prefix="${PREFIX:-$HOME/.local}"
bin="$prefix/bin/clawd"

cargo build --release
install -Dm755 target/release/clawd "$bin"
install -Dm644 assets/clawd.svg "$prefix/share/icons/hicolor/scalable/apps/dev.clawd.Clawd.svg"
install -Dm644 assets/bar-on.svg "$HOME/.local/share/clawd/bar-on.svg"
install -Dm644 assets/bar-off.svg "$HOME/.local/share/clawd/bar-off.svg"
desktop="$prefix/share/applications/dev.clawd.Clawd.desktop"
mkdir -p "$(dirname "$desktop")"
sed "s|@BIN@|$bin|g" assets/dev.clawd.Clawd.desktop.in > "$desktop"

gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true

echo "Installed $bin"
echo "Launcher entry: $desktop"
