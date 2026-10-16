#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appdir.sh" "$scratch/OneDriveForLinux.AppDir" >/dev/null
appdir="$scratch/OneDriveForLinux.AppDir"
test -x "$appdir/AppRun"
test -x "$appdir/usr/bin/onedrive-daemon"
test -x "$appdir/usr/bin/onedrive-desktop"
test -x "$appdir/usr/bin/onedrive-linux"
test -r "$appdir/.DirIcon"
test "$(find "$appdir" -maxdepth 1 -name '*.desktop' | wc -l)" -eq 1
desktop-file-validate "$appdir/io.github.onedrive_linux.desktop"
sh -n "$appdir/AppRun"
if ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appdir.sh" "$appdir" >/dev/null 2>&1; then
    echo "AppDir builder overwrote an existing directory" >&2
    exit 1
fi
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-appdir.*' | wc -l)" -eq 0

# Test launcher process ownership without opening a GTK window or D-Bus session.
mkdir -p "$scratch/fake/usr/bin"
cp "$appdir/AppRun" "$scratch/fake/AppRun"
cat > "$scratch/fake/usr/bin/onedrive-daemon" <<'EOF'
#!/bin/sh
trap 'touch "$ONEDRIVE_APPDIR_SMOKE_MARKER"; exit 0' TERM
while :; do sleep 1; done
EOF
cat > "$scratch/fake/usr/bin/onedrive-desktop" <<'EOF'
#!/bin/sh
sleep 1
exit 7
EOF
chmod +x "$scratch/fake/usr/bin/onedrive-daemon" "$scratch/fake/usr/bin/onedrive-desktop"
export ONEDRIVE_APPDIR_SMOKE_MARKER="$scratch/daemon-stopped"
if "$scratch/fake/AppRun"; then
    echo "AppRun lost the desktop exit status" >&2
    exit 1
else
    status=$?
    test "$status" -eq 7
fi
test -e "$ONEDRIVE_APPDIR_SMOKE_MARKER"
