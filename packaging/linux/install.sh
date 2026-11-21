#!/bin/sh
# Installs TimelapseMaker for the current user: the binary, a menu entry and
# the application icon. Wayland compositors (KDE Plasma, GNOME) do not accept
# the icon a client sets on its window -- they resolve it from the .desktop
# entry matching the window's app id -- so without this step the title bar and
# the taskbar show a generic icon. X11 sessions get the window icon either way.
#
# Usage: ./install.sh [path-to-timelapse]
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary=${1:-$here/timelapse}
data=${XDG_DATA_HOME:-$HOME/.local/share}
bin=${XDG_BIN_HOME:-$HOME/.local/bin}

if [ ! -f "$binary" ]; then
    printf 'No binary at %s; pass its path as the first argument.\n' "$binary" >&2
    exit 1
fi

install -Dm755 "$binary" "$bin/timelapse"
install -Dm644 "$here/timelapsemaker.desktop" "$data/applications/timelapsemaker.desktop"
for size in 16 24 32 48 64 128 256; do
    install -Dm644 "$here/icons/hicolor/${size}x${size}/apps/timelapsemaker.png" \
        "$data/icons/hicolor/${size}x${size}/apps/timelapsemaker.png"
done

# Refresh the caches when the helpers exist; a stale cache only delays the icon.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$data/icons/hicolor" >/dev/null 2>&1 || true
fi

printf 'Installed %s\n' "$bin/timelapse"
printf 'Menu entry: %s\n' "$data/applications/timelapsemaker.desktop"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) printf 'Add %s to PATH to start it from a terminal.\n' "$bin" ;;
esac
