#!/usr/bin/env bash
# Removes kerntablet and puts OpenTabletDriver back the way install.sh found it.
#   sudo ./uninstall.sh
set -euo pipefail

MARK='#kerntablet# '
STATE=/var/lib/kerntablet

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'uninstall.sh: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run it with sudo: sudo ./uninstall.sh"
USER_NAME=${SUDO_USER:-}
[ -n "$USER_NAME" ] && [ "$USER_NAME" != root ] || die "run it with sudo from your own user account"
USER_UID=$(id -u "$USER_NAME")
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
as_user() {
    runuser -u "$USER_NAME" -- env HOME="$USER_HOME" XDG_RUNTIME_DIR="/run/user/$USER_UID" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_UID/bus" "$@"
}

say "Kernel"
[ -x /usr/local/bin/kerntablet-loader ] && /usr/local/bin/kerntablet-loader detach-all || true
mountpoint -q /run/kerntablet/bpf && umount /run/kerntablet/bpf || true
rmdir /run/kerntablet/bpf /run/kerntablet 2>/dev/null || true

for f in /etc/modprobe.d/*.conf; do
    [ -f "$f" ] || continue
    if grep -qF "$MARK" "$f"; then
        sed -i "s/^${MARK}//" "$f"
        echo "$f: blacklist wacom restored"
    fi
done
OTD_MASK=/etc/modprobe.d/99-opentabletdriver.conf
if grep -qs '^# kerntablet:' "$OTD_MASK"; then
    rm -f "$OTD_MASK"
    echo "$OTD_MASK: removed"
fi
modprobe -r wacom 2>/dev/null || true

say "Files"
rm -f /usr/local/bin/kerntablet /usr/local/bin/kerntablet-loader \
      /etc/systemd/system/kerntablet@.service /etc/udev/rules.d/70-kerntablet.rules \
      /usr/local/share/applications/kerntablet.desktop
systemctl --global disable kerntablet-apply.service >/dev/null 2>&1 || true
rm -f /etc/systemd/user/kerntablet-apply.service /etc/xdg/autostart/kerntablet-apply.desktop
rm -rf /usr/local/lib/kerntablet
QUIRKS=/etc/libinput/local-overrides.quirks
if grep -qs '^# >>> kerntablet' "$QUIRKS"; then
    sed -i '/^# >>> kerntablet/,/^# <<< kerntablet/d' "$QUIRKS"
    grep -q '[^[:space:]]' "$QUIRKS" || rm -f "$QUIRKS"
fi
systemctl daemon-reload
udevadm control --reload

say "OpenTabletDriver"
swap_autostart() { # file, old, new
    local file=$1 old=$2 new=$3
    # Only where an older install.sh made the swap (it left this backup);
    # a line you added yourself stays.
    [ -f "$file" ] && [ -f "$file.bak-kerntablet" ] && grep -qF "$old" "$file" || return 0
    as_user python3 - "$file" "$old" "$new" <<'PY'
import sys
path, old, new = sys.argv[1:4]
text = open(path, encoding="utf-8").read()
open(path, "w", encoding="utf-8").write(text.replace(old, new))
PY
    echo "$file: '$old' → '$new'"
}
swap_autostart "$USER_HOME/.config/hypr/hyprland.lua" \
    'hl.exec_cmd("kerntablet apply --settle")' 'hl.exec_cmd("otd-daemon")'
swap_autostart "$USER_HOME/.config/i3/config" \
    'exec --no-startup-id kerntablet apply --settle' 'exec --no-startup-id otd-daemon'

HYPR_LUA="$USER_HOME/.config/hypr/hyprland.lua"
if [ -f "$HYPR_LUA" ] && grep -qF 'hypr/kerntablet.lua' "$HYPR_LUA"; then
    as_user sed -i -e '/^-- kerntablet: /d' -e '/hypr\/kerntablet\.lua/d' "$HYPR_LUA"
    echo "$HYPR_LUA: kerntablet.lua no longer sourced"
fi
rm -f "$USER_HOME/.config/hypr/kerntablet.lua"

if [ -e "$STATE/otd-service-was-enabled" ]; then
    as_user systemctl --user enable --now opentabletdriver.service >/dev/null 2>&1 || true
    echo "opentabletdriver service re-enabled"
fi
rm -rf "$STATE"

say "Done"
echo "kerntablet settings were kept in $USER_HOME/.config/kerntablet. Replug the tablet."
