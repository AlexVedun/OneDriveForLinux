#!/bin/sh
set -eu

daemon="${1:-target/debug/onedrive-daemon}"
"$daemon" &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true' EXIT HUP INT TERM

attempt=0
while [ "$attempt" -lt 10 ]; do
    if result=$(gdbus call --session \
        --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.Status 2>/dev/null); then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done

test "${result:-}" = "('needs-login',)"

# A second client can still query the daemon after the first one disconnects.
result=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.Status)
test "$result" = "('needs-login',)"
kill -0 "$daemon_pid"
