#!/bin/sh
set -eu

command -v gtk4-broadwayd >/dev/null
test -n "${DBUS_SESSION_BUS_ADDRESS:-}"

test_state=$(mktemp -d)
display_number=$((300 + $$ % 1000))
export XDG_STATE_HOME="$test_state/state"
export ONEDRIVE_CLIENT_ID=
export GDK_BACKEND=broadway
export BROADWAY_DISPLAY=:$display_number
mkdir "$XDG_STATE_HOME"

gtk4-broadwayd --address=127.0.0.1 "$BROADWAY_DISPLAY" \
    >"$test_state/broadway.log" 2>&1 &
broadway_pid=$!
target/debug/onedrive-desktop >"$test_state/desktop.log" 2>&1 &
desktop_pid=$!
trap 'kill "$desktop_pid" "$broadway_pid" 2>/dev/null || true; rm -rf "$test_state"' EXIT HUP INT TERM

attempt=0
while [ "$attempt" -lt 10 ]; do
    service=org.kde.StatusNotifierItem-$desktop_pid-1
    if gdbus introspect --session --dest "$service" \
        --object-path /StatusNotifierItem >/dev/null 2>&1; then
        break
    fi
    kill -0 "$desktop_pid"
    attempt=$((attempt + 1))
    sleep 1
done
test "$attempt" -lt 10

for interface in org.kde.StatusNotifierItem org.freedesktop.StatusNotifierItem; do
    service=$interface-$desktop_pid-1
    status=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" Status)
    test "$status" = "(<'NeedsAttention'>,)"
    icon=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" IconName)
    test "$icon" = "(<'dialog-warning-symbolic'>,)"
    tooltip=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" ToolTip)
    case "$tooltip" in
        *"Фоновый процесс недоступен"*) ;;
        *) exit 1 ;;
    esac
    gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method "$interface".Activate 0 0 >/dev/null
done
