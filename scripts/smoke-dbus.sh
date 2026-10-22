#!/bin/sh
set -eu

daemon="${1:-target/debug/onedrive-daemon}"
test_state=$(mktemp -d)
export XDG_STATE_HOME="$test_state"
export ONEDRIVE_SMOKE_NO_RESTORE=1
case "${SMOKE_TEST_LOGIN:-0}" in
    1) export ONEDRIVE_CLIENT_ID=test-client ;;
    xdg)
        export XDG_CONFIG_HOME="$test_state/config"
        mkdir -p "$XDG_CONFIG_HOME/onedrive-linux"
        printf 'ONEDRIVE_CLIENT_ID=test-client\n' > "$XDG_CONFIG_HOME/onedrive-linux/config.env"
        unset ONEDRIVE_CLIENT_ID
        ;;
    *)
        # An explicit empty value prevents startup credential restoration.
        export ONEDRIVE_CLIENT_ID=
        ;;
esac
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
revision=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ApiRevision)
test "$revision" = "(uint32 2,)"

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

gdbus introspect --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon | grep -q PrepareVirtualFile
gdbus introspect --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon | grep -q MountVirtualDrive
gdbus introspect --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon | grep -q ReconcileVirtualDraft
cache_stats=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualCacheStats)
test "$cache_stats" = "(uint64 0, uint64 1073741824)"
cache_stats=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetVirtualCacheLimit 268435456)
test "$cache_stats" = "(uint64 0, uint64 268435456)"
cache_stats=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualCacheStats)
test "$cache_stats" = "(uint64 0, uint64 268435456)"
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetVirtualCacheLimit 1048576 >/dev/null 2>&1; then
    exit 1
fi
gdbus introspect --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon | grep -q SetVirtualFilePinned
mount_path=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualMountPath)
test "$mount_path" = "('',)"
recovery_path=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualMountRecoveryPath)
test "$recovery_path" = "('',)"
draft_count=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualDraftCount)
test "$draft_count" = "(uint64 0,)"
drafts=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualDrafts)
test "$drafts" = "('[]',)"
mutations=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualMutations)
test "$mutations" = "('[]',)"
directories=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualDirectories)
test "$directories" = "('[]',)"
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.CancelConflictedVirtualMutation drive missing >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ConfirmVirtualMassDelete 11 >/dev/null 2>&1; then
    exit 1
fi
upload_enabled=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualDraftUploadEnabled)
test "$upload_enabled" = "(true,)"
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ExportVirtualDraft drive missing /tmp/missing-draft-copy >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunVirtualDraftUpload item >/dev/null 2>&1; then
    exit 1
fi
fuse_diagnostics=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualFuseDiagnostics)
case "$fuse_diagnostics" in
    "('ready', true)"|"('ready', false)"|"('device-missing', true)"|"('device-missing', false)"|"('device-permission', true)"|"('device-permission', false)"|"('device-error', true)"|"('device-error', false)") ;;
    *) exit 1 ;;
esac
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.MountVirtualDrive relative >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.UnmountVirtualDrive >/dev/null 2>&1; then
    exit 1
fi
mount_path=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualMountPath)
test "$mount_path" = "('',)"
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.PrepareVirtualFile missing >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.VirtualFileCacheStatus missing >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetVirtualFilePinned missing false >/dev/null 2>&1; then
    exit 1
fi

root=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncRoot)
test "$root" = "('',)"

mkdir "$test_state/sync"
root=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SetSyncRoot "$test_state/sync")
test "$root" = "('$test_state/sync',)"
saved_root=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncRoot)
test "$saved_root" = "$root"
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
outstanding=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.SyncOutstandingCount)
test "$outstanding" = "(uint64 0,)"
review_items=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.ReviewOperations)
test "$review_items" = "('[]',)"
history=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.OperationHistory)
test "$history" = "('{\"total\":0,\"operations\":[]}',)"
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
    --method io.github.onedrive_linux.Daemon.RunQueuedRemoteRename >/dev/null 2>&1; then
    exit 1
fi
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedRemoteDelete >/dev/null 2>&1; then
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
if gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.RunQueuedLocalDelete >/dev/null 2>&1; then
    exit 1
fi

if [ "${SMOKE_TEST_LOGIN:-0}" != 1 ] && [ "${SMOKE_TEST_LOGIN:-0}" != xdg ]; then
    exit 0
fi

login_url=$(gdbus call --session \
    --dest io.github.onedrive_linux.Daemon \
    --object-path /io/github/onedrive_linux/Daemon \
    --method io.github.onedrive_linux.Daemon.BeginLogin)
case "$login_url" in
    *login.microsoftonline.com*client_id=test-client*code_challenge_method=S256*) ;;
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
