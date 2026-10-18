#!/bin/sh
set -eu

command -v gtk4-broadwayd >/dev/null
test -n "${DBUS_SESSION_BUS_ADDRESS:-}"
binary_dir=${ONEDRIVE_SMOKE_BINARY_DIR:-target/debug}

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
"$binary_dir/onedrive-desktop" >"$test_state/desktop.log" 2>&1 &
desktop_pid=$!
daemon_pid=
trap 'kill "$desktop_pid" "$broadway_pid" ${daemon_pid:+"$daemon_pid"} 2>/dev/null || true; rm -rf "$test_state"' EXIT HUP INT TERM

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
    test "$icon" = "(<''>,)"
    pixmap=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" IconPixmap)
    case "$pixmap" in
        *"(22, 22,"*"(32, 32,"*) ;;
        *) exit 1 ;;
    esac
    attention=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" AttentionIconPixmap)
    case "$attention" in
        *"(22, 22,"*"(32, 32,"*) ;;
        *) exit 1 ;;
    esac
    tooltip=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" ToolTip)
    case "$tooltip" in
        *"Фоновый процесс недоступен"*) ;;
        *) exit 1 ;;
    esac
    menu=$(gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get "$interface" Menu)
    test "$menu" = "(<objectpath '/Menu'>,)"
    layout=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetLayout -- 0 -1 "[]")
    for label in 'Открыть окно' 'Открыть папку синхронизации' 'Очередь и история' 'Приостановить синхронизацию' 'Проверить OneDrive' 'Выход'; do
        case "$layout" in
            *"$label"*) ;;
            *) exit 1 ;;
        esac
    done
    pause_enabled=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 3 enabled)
    test "$pause_enabled" = '(<false>,)'
    folder_enabled=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 6 enabled)
    test "$folder_enabled" = '(<false>,)'
    queue_label=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 2 label)
    test "$queue_label" = "(<'Очередь и история'>,)"
    gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.AboutToShow 0 >/dev/null
    gdbus call --session --dest "$service" \
        --object-path /StatusNotifierItem \
        --method "$interface".Activate 0 0 >/dev/null
done

"$binary_dir/onedrive-daemon" >"$test_state/daemon.log" 2>&1 &
daemon_pid=$!
attempt=0
while [ "$attempt" -lt 10 ]; do
    if gdbus call --session --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.SyncPaused >/dev/null 2>&1; then
        break
    fi
    kill -0 "$daemon_pid"
    attempt=$((attempt + 1))
    sleep 1
done
test "$attempt" -lt 10

mkdir "$test_state/sync"
root=$(gdbus call --session --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetSyncRoot "$test_state/sync")
test "$root" = "('$test_state/sync',)"

attempt=0
while [ "$attempt" -lt 15 ]; do
    pause_enabled=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 3 enabled 2>/dev/null || true)
    if [ -z "$pause_enabled" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$attempt" -lt 15

attempt=0
while [ "$attempt" -lt 15 ]; do
    folder_enabled=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 6 enabled 2>/dev/null || true)
    if [ -z "$folder_enabled" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$attempt" -lt 15

gdbus call --session --dest "$service" \
    --object-path /Menu --method com.canonical.dbusmenu.Event 3 clicked "<''>" 0 >/dev/null
attempt=0
while [ "$attempt" -lt 10 ]; do
    paused=$(gdbus call --session --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.SyncPaused)
    if [ "$paused" = '(true,)' ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$paused" = '(true,)'
attempt=0
while [ "$attempt" -lt 10 ]; do
    label=$(gdbus call --session --dest "$service" \
        --object-path /Menu --method com.canonical.dbusmenu.GetProperty 3 label)
    if [ "$label" = "(<'Возобновить синхронизацию'>,)" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$label" = "(<'Возобновить синхронизацию'>,)"

gdbus call --session --dest "$service" \
    --object-path /Menu --method com.canonical.dbusmenu.Event 5 clicked "<''>" 0 >/dev/null
attempt=0
while kill -0 "$desktop_pid" 2>/dev/null && [ "$attempt" -lt 10 ]; do
    attempt=$((attempt + 1))
    sleep 1
done
! kill -0 "$desktop_pid" 2>/dev/null
