#!/usr/bin/env bash
# Install or update fanctlrs from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/diogomartino/fanctlrs/main/install.sh | bash
#   ... | bash -s -- --uninstall
#
# Installs for the current user only (no sudo). The system part (fan driver,
# permissions) is done from the app's Setup screen.
set -euo pipefail

REPO="diogomartino/fanctlrs"
BIN="$HOME/.local/bin/fanctlrs"
DATA="$HOME/.local/share/fanctlrs"
DESKTOP="$HOME/.local/share/applications/fanctlrs.desktop"
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/fanctlrs.service"

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || die "run this as your normal user, not with sudo"
[ "$(uname -s)" = Linux ] || die "fanctlrs only runs on Linux"
[ "$(uname -m)" = x86_64 ] || die "no release build for $(uname -m), build it from source instead"
command -v curl >/dev/null || die "curl is required"

service_active() { systemctl --user is-active --quiet fanctlrs 2>/dev/null; }

if [ "${1:-}" = "--uninstall" ]; then
    say "Uninstalling fanctlrs"
    systemctl --user disable --now fanctlrs 2>/dev/null || true
    rm -f "$UNIT" "$BIN" "$DESKTOP"
    rm -rf "$DATA"
    systemctl --user daemon-reload 2>/dev/null || true
    say "Done. Your settings are kept in ~/.config/fanctlrs (delete it for a clean slate)."
    echo "System files from the app's Fix button, if you used it:"
    echo "  sudo rm -f /etc/udev/rules.d/99-fanctlrs.rules /etc/modules-load.d/fanctlrs.conf /etc/modprobe.d/fanctlrs.conf"
    exit 0
fi

# Latest release: its tag and the archive for this machine.
release=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest") \
    || die "couldn't reach GitHub, or there's no release yet"
tag=$(grep -m1 '"tag_name"' <<<"$release" | cut -d'"' -f4)
url=${FANCTLRS_URL:-$(grep -o '"browser_download_url": *"[^"]*x86_64-linux\.tar\.gz"' <<<"$release" | cut -d'"' -f4 | head -1)}
[ -n "$tag" ] && [ -n "$url" ] || die "the latest release has no x86_64 Linux build"

installed=$(cat "$DATA/version" 2>/dev/null || true)
if [ "$installed" = "$tag" ] && [ -x "$BIN" ]; then
    say "fanctlrs $tag is already installed and up to date."
    exit 0
fi

if [ -n "$installed" ]; then say "Updating fanctlrs $installed -> $tag"; else say "Installing fanctlrs $tag"; fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fL --progress-bar "$url" -o "$tmp/fanctlrs.tar.gz"
tar -xzf "$tmp/fanctlrs.tar.gz" -C "$tmp"
[ -f "$tmp/fanctlrs" ] || die "the archive doesn't contain fanctlrs"

# Rename into place: works even while the old version is running.
mkdir -p "$(dirname "$BIN")" "$DATA" "$(dirname "$DESKTOP")"
install -m 755 "$tmp/fanctlrs" "$BIN.new"
mv -f "$BIN.new" "$BIN"
echo "$tag" > "$DATA/version"

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=fanctlrs
Comment=Watch sensors and control fans
Exec=$BIN
Icon=utilities-system-monitor
Categories=System;Monitor;
EOF

if service_active; then
    systemctl --user restart fanctlrs
    say "Background service restarted on $tag."
fi
if pgrep -fx "$BIN" >/dev/null; then
    echo "fanctlrs is open: close and reopen it to use $tag."
fi

say "Installed fanctlrs $tag to $BIN"
case ":$PATH:" in
    *":$HOME/.local/bin:"*) echo "Start it from your app menu or run: fanctlrs" ;;
    *) echo "Start it from your app menu or run: $BIN" ;;
esac
