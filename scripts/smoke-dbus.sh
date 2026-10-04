#!/bin/sh
set -eu

daemon="${1:-target/debug/onedrive-daemon}"
test_state=$(mktemp -d)
export XDG_STATE_HOME="$test_state"
if [ "${SMOKE_TEST_LOGIN:-0}" = 1 ]; then
    export ONEDRIVE_CLIENT_ID=test-client
else
    # An explicit empty value prevents startup credential restoration.
    export ONEDRIVE_CLIENT_ID=
fi
"$daemon" &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true; rm -rf "$test_state"' EXIT HUP INT TERM

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

items=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.CatalogItems)
test "$items" = "('[]',)"

mkdir "$test_state/sync"
root=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetSyncRoot "$test_state/sync")
test "$root" = "('$test_state/sync',)"
attempt=0
while [ "$attempt" -lt 10 ]; do
    epoch=$(gdbus call --session \
        --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.LocalChangeEpoch)
    if [ "$epoch" != "(uint64 0,)" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$epoch" != "(uint64 0,)"
printf 'changed' > "$test_state/sync/new.txt"
attempt=0
while [ "$attempt" -lt 10 ]; do
    changed=$(gdbus call --session \
        --dest io.github.onedrive_linux.Daemon \
        --object-path /io/github/onedrive_linux/Daemon \
        --method io.github.onedrive_linux.Daemon.LocalChangeEpoch)
    if [ "$changed" != "$epoch" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
test "$changed" != "$epoch"
paused=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncPaused)
test "$paused" = "(false,)"
reviews=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncReviewCount)
test "$reviews" = "(uint64 0,)"
review_items=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ReviewOperations)
test "$review_items" = "('[]',)"
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ApproveOperation 1 >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.DismissOperation 1 >/dev/null 2>&1; then
    exit 1
fi
gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetSyncPaused true >/dev/null
paused=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncPaused)
test "$paused" = "(true,)"
gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetSyncPaused false >/dev/null
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.PreviewSync >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueSafeUploads >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueSafeDownloads >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.PrepareLocalFolders >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueSafeRemoteFolders >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueSafeLocalRenames >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueReviewActions >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.QueueUploadNew "new.txt" >/dev/null 2>&1; then
    exit 1
fi

# The experimental write path stays disabled in the normal daemon.
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedUpload >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedRemoteFolder >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedDownload >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedLocalRename >/dev/null 2>&1; then
    exit 1
fi

if [ "${SMOKE_TEST_LOGIN:-0}" != 1 ]; then
    exit 0
fi

login_url=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.BeginLogin)
case "$login_url" in
    *login.microsoftonline.com*code_challenge_method=S256*) ;;
    *) exit 1 ;;
esac

# A second login must not replace the pending PKCE challenge.
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.BeginLogin >/dev/null 2>&1; then
    exit 1
fi
kill -0 "$daemon_pid"
