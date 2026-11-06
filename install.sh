#!/bin/sh
# Install Ligato Studio for the current user (no root needed).
#
# From GitHub (downloads the latest prebuilt release):
#   curl -fsSL https://raw.githubusercontent.com/raevibe/ligato-studio/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/raevibe/ligato-studio/main/install.sh | sh -s -- --all-devices
#
# From a source checkout (uses target/release if built, or builds it with cargo):
#   ./install.sh [--all-devices]
#
#   --all-devices   launcher also lists non-sc0710 capture devices (e.g. a USB Cam Link)
#   --from-source   build with cargo instead of downloading a release
#   --uninstall     remove everything this script installed
set -eu

REPO="raevibe/ligato-studio"
ASSET="ligato-studio-x86_64-linux.tar.gz"

all_devices=""
from_source=""
uninstall=""
for arg in "$@"; do
    case "$arg" in
        --all-devices) all_devices=" --all-devices" ;;
        --from-source) from_source=1 ;;
        --uninstall) uninstall=1 ;;
        -h|--help) sed -n '2,14p' "$0" 2>/dev/null || true; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; exit 2 ;;
    esac
done

data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_dir="$HOME/.local/bin"
app_dir="$data/applications"
icon_root="$data/icons/hicolor"
bin="$bin_dir/sc0710-studio"
desktop="$app_dir/sc0710-studio.desktop"
icon="$icon_root/scalable/apps/sc0710-studio.svg"

say() { printf '%s\n' "$*"; }
refresh() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$app_dir" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$icon_root" 2>/dev/null || true
}

if [ -n "$uninstall" ]; then
    rm -f "$bin" "$desktop" "$icon"
    refresh
    say "Removed Ligato Studio. Settings in ~/.config/sc0710-studio were kept."
    exit 0
fi

# Where are the files? A source checkout next to this script, or a downloaded release.
src=""
here=""
case "$0" in */*) here=$(cd "$(dirname "$0")" && pwd) ;; esac
tmp=""
cleanup() { if [ -n "$tmp" ]; then rm -rf "$tmp"; fi; }
trap cleanup EXIT

if [ -n "$here" ] && [ -f "$here/Cargo.toml" ] && [ -f "$here/contrib/sc0710-studio.desktop" ]; then
    src="$here"
    if [ -n "$from_source" ] || [ ! -x "$src/target/release/sc0710-studio" ]; then
        if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
        command -v cargo >/dev/null 2>&1 || { say "cargo not found. Install Rust from https://rustup.rs and re-run." >&2; exit 1; }
        say "Building (cargo build --release)..."
        (cd "$src" && cargo build --release)
    fi
    binary="$src/target/release/sc0710-studio"
else
    case "$(uname -m)" in
        x86_64) ;;
        *) say "Prebuilt releases are x86_64 only. Clone the repo and run ./install.sh --from-source." >&2; exit 1 ;;
    esac
    command -v curl >/dev/null 2>&1 || { say "curl is required." >&2; exit 1; }
    tmp=$(mktemp -d)
    url="${LIGATO_STUDIO_URL:-https://github.com/$REPO/releases/latest/download/$ASSET}"
    say "Downloading $url"
    curl -fL --progress-bar "$url" -o "$tmp/$ASSET"
    if command -v sha256sum >/dev/null 2>&1 && curl -fsSL "$url.sha256" -o "$tmp/$ASSET.sha256"; then
        (cd "$tmp" && sha256sum -c --quiet "$ASSET.sha256") || { say "Checksum mismatch, aborting." >&2; exit 1; }
    fi
    tar -xzf "$tmp/$ASSET" -C "$tmp"
    src="$tmp/ligato-studio"
    binary="$src/sc0710-studio"
fi

mkdir -p "$bin_dir" "$app_dir" "$(dirname "$icon")"
install -m 755 "$binary" "$bin"
install -m 644 "$src/contrib/sc0710-studio.svg" "$icon"
# Absolute Exec path: ~/.local/bin is not always on the desktop session's PATH.
sed -e "s|^Exec=sc0710-studio --all-devices|Exec=$bin --all-devices|" \
    -e "s|^Exec=sc0710-studio\$|Exec=$bin$all_devices|" \
    "$src/contrib/sc0710-studio.desktop" > "$desktop"
chmod 644 "$desktop"
refresh

say "Installed Ligato Studio to $bin. Look for it in your applications menu."

# Runtime checks: things the app needs that this script can't install without root.
missing=""
command -v gst-launch-1.0 >/dev/null 2>&1 || missing="$missing gstreamer"
if [ -n "$missing" ]; then
    say ""
    say "Recording, audio and encoder detection need GStreamer, which isn't installed:"
    say "  Debian/Ubuntu/Pop!_OS: sudo apt install gstreamer1.0-tools gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-alsa"
    say "  Fedora:                sudo dnf install gstreamer1 gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-ugly-free"
    say "  Arch:                  sudo pacman -S gstreamer gst-plugins-good gst-plugins-bad gst-plugins-ugly"
fi
if [ ! -d /sys/module/sc0710 ]; then
    say ""
    say "The sc0710 driver isn't loaded. Install it from https://github.com/Nakildias/sc0710"
    [ -z "$all_devices" ] && say "(or reinstall with --all-devices to use other capture devices)."
fi
# Desktop sessions usually get camera access from logind; only warn if no node is usable.
nodes=0; usable=0
for n in /dev/video*; do
    [ -e "$n" ] || continue
    nodes=$((nodes + 1))
    if [ -r "$n" ] && [ -w "$n" ]; then usable=$((usable + 1)); fi
done
if [ "$nodes" -gt 0 ] && [ "$usable" -eq 0 ]; then
    say ""
    say "You can't open /dev/video*: sudo usermod -aG video \"\$USER\", then log out and back in."
fi
exit 0
