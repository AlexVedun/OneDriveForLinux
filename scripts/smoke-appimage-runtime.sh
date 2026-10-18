#!/bin/sh
set -eu

if [ "$#" -ne 1 ] || [ ! -x "$1" ]; then
    echo "Usage: dbus-run-session -- sh $0 FILE.AppImage" >&2
    exit 2
fi
test -n "${DBUS_SESSION_BUS_ADDRESS:-}"
command -v gtk4-broadwayd >/dev/null

image=$(realpath -- "$1")
test_state=$(mktemp -d)
display_number=$((300 + $$ % 1000))
export XDG_CONFIG_HOME="$test_state/config"
export XDG_DATA_HOME="$test_state/data"
export XDG_STATE_HOME="$test_state/state"
export XDG_CACHE_HOME="$test_state/cache"
export ONEDRIVE_CLIENT_ID=
export GDK_BACKEND=broadway
export BROADWAY_DISPLAY=:$display_number
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"

gtk4-broadwayd --address=127.0.0.1 "$BROADWAY_DISPLAY" \
    >"$test_state/broadway.log" 2>&1 &
broadway_pid=$!
python3 -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' \
    "$image" --appimage-extract-and-run >"$test_state/appimage.log" 2>&1 &
image_pid=$!
cleanup() {
    kill -TERM -- "-$image_pid" 2>/dev/null || true
    kill "$broadway_pid" 2>/dev/null || true
    wait "$image_pid" 2>/dev/null || true
    wait "$broadway_pid" 2>/dev/null || true
    rm -rf "$test_state"
}
trap cleanup EXIT HUP INT TERM

attempt=0
daemon_ready=false
tray_ready=false
while [ "$attempt" -lt 25 ]; do
    if gdbus call --session --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.SyncPaused >/dev/null 2>&1 && \
       gdbus call --session --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.VirtualConfiguredMountPath >/dev/null 2>&1; then
        daemon_ready=true
    fi
    names=$(gdbus call --session --dest org.freedesktop.DBus \
        --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.ListNames)
    case "$names" in
        *org.kde.StatusNotifierItem-*) tray_ready=true ;;
    esac
    if [ "$daemon_ready" = true ] && [ "$tray_ready" = true ]; then
        exit 0
    fi
    if ! kill -0 "$image_pid" 2>/dev/null; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
echo "AppImage did not start both the daemon and tray" >&2
cat "$test_state/appimage.log" >&2
cat "$test_state/broadway.log" >&2
exit 1
