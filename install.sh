#!/usr/bin/env bash
# kerntablet installer. Build first as your user (cargo build --release),
# then:  sudo ./install.sh
#
# What it does:
#   1. installs the two binaries, a udev rule, a systemd unit, a libinput quirk
#   2. switches the tablet from OpenTabletDriver to the kernel `wacom` driver
#      (un-blacklists the module, stops OTD)
#   3. sets up `kerntablet apply --settle` at every graphical login: a systemd
#      user unit (graphical-session.target) plus an XDG autostart entry for
#      desktops without systemd. Your own config files are never edited.
#   4. attaches the HID-BPF transform (and installed plugins) to the tablet
# Everything is undone by:  sudo ./uninstall.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/target/release"
MARK='#kerntablet# '
STATE=/var/lib/kerntablet
GROUP=input

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run it with sudo: sudo ./install.sh"
USER_NAME=${SUDO_USER:-}
[ -n "$USER_NAME" ] && [ "$USER_NAME" != root ] || die "run it with sudo from your own user account"
[ -x "$BIN/kerntablet" ] && [ -x "$BIN/kerntablet-loader" ] || die "build first: cargo build --release"
USER_UID=$(id -u "$USER_NAME")
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
as_user() {
    runuser -u "$USER_NAME" -- env HOME="$USER_HOME" XDG_RUNTIME_DIR="/run/user/$USER_UID" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_UID/bus" "$@"
}

grep -q 'CONFIG_HID_BPF=y' "/usr/lib/modules/$(uname -r)/build/.config" 2>/dev/null \
    || zcat /proc/config.gz 2>/dev/null | grep -q 'CONFIG_HID_BPF=y' \
    || die "the kernel is built without CONFIG_HID_BPF"

say "Files"
install -Dm755 "$BIN/kerntablet"        /usr/local/bin/kerntablet
install -Dm755 "$BIN/kerntablet-loader" /usr/local/bin/kerntablet-loader
install -Dm644 "$HERE/dist/kerntablet@.service" /etc/systemd/system/kerntablet@.service
install -Dm644 "$HERE/dist/70-kerntablet.rules" /etc/udev/rules.d/70-kerntablet.rules
install -Dm644 "$HERE/dist/kerntablet.desktop"  /usr/local/share/applications/kerntablet.desktop
install -d -m 2775 -g "$GROUP" "$STATE"
# Plugins are loaded into the kernel by the root loader: root-only directory.
install -d -m 755 -o root -g root /usr/local/lib/kerntablet /usr/local/lib/kerntablet/plugins

QUIRKS=/etc/libinput/local-overrides.quirks
install -d /etc/libinput
# Replace our block on every run so newly supported models get the quirk too.
if grep -qs '^# >>> kerntablet' "$QUIRKS"; then
    sed -i '/^# >>> kerntablet/,/^# <<< kerntablet/d' "$QUIRKS"
fi
[ -s "$QUIRKS" ] && [ -n "$(tail -c1 "$QUIRKS")" ] && printf '\n' >> "$QUIRKS"
cat "$HERE/dist/kerntablet.quirks" >> "$QUIRKS"
echo "libinput: pen smoothing disabled ($QUIRKS)"

if ! id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx "$GROUP"; then
    usermod -aG "$GROUP" "$USER_NAME"
    echo "NOTE: $USER_NAME was added to the $GROUP group — changing the area without root works after you log in again"
fi

say "OpenTabletDriver → kernel wacom driver"
if as_user systemctl --user is-enabled opentabletdriver.service >/dev/null 2>&1; then
    touch "$STATE/otd-service-was-enabled"
fi
as_user systemctl --user disable --now opentabletdriver.service >/dev/null 2>&1 || true
pkill -u "$USER_NAME" -f 'OpenTabletDriver\.Daemon' 2>/dev/null || true

# `blacklist wacom` (added for OTD) -> commented out with a marker
for f in /etc/modprobe.d/*.conf; do
    [ -f "$f" ] || continue
    if grep -qE '^[[:space:]]*blacklist[[:space:]]+wacom[[:space:]]*$' "$f"; then
        sed -i -E "s/^([[:space:]]*blacklist[[:space:]]+wacom[[:space:]]*)\$/${MARK}\\1/" "$f"
        echo "$f: blacklist wacom disabled"
    fi
done

# The OpenTabletDriver package also turns `modprobe wacom` into a no-op
# (/usr/lib/modprobe.d/99-opentabletdriver.conf). A file with the same name
# in /etc replaces it; every line except the wacom one is kept.
OTD_MODPROBE=/usr/lib/modprobe.d/99-opentabletdriver.conf
OTD_MASK=/etc/modprobe.d/99-opentabletdriver.conf
if grep -qsE '^[[:space:]]*install[[:space:]]+wacom[[:space:]]' "$OTD_MODPROBE" && [ ! -e "$OTD_MASK" ]; then
    {
        echo "# kerntablet: overrides $OTD_MODPROBE so the wacom module can load"
        grep -vE '^[[:space:]]*install[[:space:]]+wacom[[:space:]]' "$OTD_MODPROBE" || true
    } > "$OTD_MASK"
    echo "$OTD_MASK: 'install wacom /usr/bin/true' from the OTD package overridden"
fi

say "Session autostart"
install -Dm644 "$HERE/dist/kerntablet-apply.service" /etc/systemd/user/kerntablet-apply.service
install -Dm644 "$HERE/dist/kerntablet-apply.desktop" /etc/xdg/autostart/kerntablet-apply.desktop
systemctl --global enable kerntablet-apply.service >/dev/null
echo "kerntablet-apply.service: enabled for graphical-session.target (all users)"
echo "/etc/xdg/autostart/kerntablet-apply.desktop: for desktops that run XDG autostart themselves"

# Compositor / X session files where people start things by hand. Only read.
AUTOSTART_FILES=(
    "$USER_HOME/.config/hypr" "$USER_HOME/.config/sway" "$USER_HOME/.config/i3"
    "$USER_HOME/.config/niri" "$USER_HOME/.config/river" "$USER_HOME/.config/labwc"
    "$USER_HOME/.config/wayfire.ini" "$USER_HOME/.config/autostart"
    "$USER_HOME/.xinitrc" "$USER_HOME/.xprofile" "$USER_HOME/.xsession"
)
scan() { # pattern
    local f
    for f in "${AUTOSTART_FILES[@]}"; do
        [ -e "$f" ] || continue
        grep -rnIE --exclude='*.bak*' --exclude='*~' --exclude='*.orig' --exclude='*.old' "$1" "$f" 2>/dev/null | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(#|--)' || true
    done
}
OTD_LINES=$(scan 'otd-daemon|OpenTabletDriver\.Daemon')
if [ -n "$OTD_LINES" ]; then
    echo
    echo "WARNING: your config still starts OpenTabletDriver, which would take the tablet"
    echo "away from the kernel driver at the next login (if that file is in use). Remove these lines yourself:"
    printf '  %s\n' "$OTD_LINES"
fi
if as_user systemctl --user is-active --quiet graphical-session.target; then
    OWN_LINES=$(scan 'kerntablet apply')
    if [ -n "$OWN_LINES" ]; then
        echo
        echo "Your session reaches graphical-session.target, so kerntablet-apply.service"
        echo "already runs at login. These lines are no longer needed (harmless if kept):"
        printf '  %s\n' "$OWN_LINES"
    fi
elif [ -z "$(scan 'kerntablet apply')" ]; then
    echo
    echo "NOTE: this session does not reach graphical-session.target (a compositor or"
    echo "window manager started without systemd/uwsm). Add to its autostart yourself:"
    echo "  kerntablet apply --settle"
    echo "(Hyprland: hl.exec_cmd(\"kerntablet apply --settle\") / exec-once = …,"
    echo " sway / i3: exec kerntablet apply --settle)"
fi

say "Kernel"
systemctl daemon-reload
udevadm control --reload
modprobe wacom
[ -d /sys/module/wacom ] || die "the wacom module did not load (modprobe wacom did nothing — see modprobe -c | grep wacom)"
udevadm settle --timeout=5 || true
for _ in 1 2 3 4 5 6 7 8 9 10; do
    ls /sys/bus/hid/drivers/wacom/ 2>/dev/null | grep -q '^0003:056A:' && break
    sleep 0.3
done
/usr/local/bin/kerntablet-loader attach-all --group "$GROUP" --force
echo
/usr/local/bin/kerntablet-loader status

say "Done"
cat <<MSG
Run "kerntablet" (as your user), check the areas and press Apply.
Filters are plugins: kerntablet plugin install <dir>, then the Filters tab.
  Writing one: PLUGINS.md
Disabling libinput smoothing takes effect after you restart the session.
To go back to OpenTabletDriver: sudo ./uninstall.sh
MSG
