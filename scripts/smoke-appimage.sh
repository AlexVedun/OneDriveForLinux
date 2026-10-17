#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

if APPIMAGETOOL=false ONEDRIVE_APPDIR_PROFILE=dev \
    sh "$repo/scripts/build-appimage.sh" "$scratch/failed.AppImage" >/dev/null 2>&1; then
    echo "Builder accepted a failed appimagetool run" >&2
    exit 1
fi
test ! -e "$scratch/failed.AppImage"
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-appimage.*' | wc -l)" -eq 0

ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appimage.sh" "$scratch/OneDriveForLinux.AppImage" >"$scratch/build.log"
image="$scratch/OneDriveForLinux.AppImage"
test -x "$image"
readelf -h "$image" >/dev/null
if ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appimage.sh" "$image" >/dev/null 2>&1; then
    echo "Builder overwrote an existing AppImage" >&2
    exit 1
fi
cd "$scratch"
"$image" --appimage-extract >/dev/null
extracted="$scratch/squashfs-root"
test -x "$extracted/AppRun"
test -x "$extracted/usr/bin/onedrive-desktop"
test -s "$extracted/usr/share/glib-2.0/schemas/gschemas.compiled"
test -f "$extracted/usr/share/icons/Adwaita/index.theme"
test -f "$extracted/usr/share/metainfo/io.github.onedrive_linux.appdata.xml"
test -s "$extracted/usr/share/licenses/onedrive-linux/native-libraries.json"
cmp "$repo/LICENSE" "$extracted/usr/share/licenses/onedrive-linux/LICENSE"
sh "$repo/scripts/audit-appdir-libs.sh" "$extracted" >"$scratch/audit.log"
grep -q '^  BUNDLED libgtk-4.so.1 ' "$scratch/audit.log"
dbus-run-session -- sh "$repo/scripts/smoke-appimage-runtime.sh" "$image"
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-appimage.*' | wc -l)" -eq 0
