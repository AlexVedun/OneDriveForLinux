#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
    echo "Usage: LINUXDEPLOY=/path/to/linuxdeploy APPIMAGETOOL=/path/to/appimagetool $0 [OUTPUT.AppImage]" >&2
    exit 2
fi

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sh "$repo/scripts/app-version.sh")
export VERSION="$version"
requested_output=${1:-"$repo/target/dist/OneDriveForLinux-$version-x86_64.AppImage"}
case "$requested_output" in
    /*) output=$requested_output ;;
    *) output="$PWD/$requested_output" ;;
esac
output=$(realpath -m -- "$output")
case "$output" in
    *.AppImage) ;;
    *) echo "Output must end in .AppImage: $output" >&2; exit 2 ;;
esac
if [ -e "$output" ] || [ -L "$output" ]; then
    echo "AppImage already exists: $output" >&2
    exit 2
fi

tool=${APPIMAGETOOL:-appimagetool}
if ! command -v "$tool" >/dev/null 2>&1; then
    echo "appimagetool is unavailable; set APPIMAGETOOL to its executable path" >&2
    exit 2
fi
case "$tool" in
    *.AppImage) set -- --appimage-extract-and-run ;;
    *) set -- ;;
esac
if [ -n "${APPIMAGETOOL_RUNTIME_FILE:-}" ]; then
    if [ ! -f "$APPIMAGETOOL_RUNTIME_FILE" ]; then
        echo "AppImage runtime is unavailable: $APPIMAGETOOL_RUNTIME_FILE" >&2
        exit 2
    fi
    set -- "$@" --runtime-file="$APPIMAGETOOL_RUNTIME_FILE"
fi

mkdir -p "$(dirname -- "$output")"
staging=$(mktemp -d "$(dirname -- "$output")/.onedrive-appimage.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
sh "$repo/scripts/build-appdir.sh" "$staging/input.AppDir" >/dev/null
sh "$repo/scripts/bundle-appdir-libs.sh" "$staging/input.AppDir" "$staging/bundled.AppDir" >/dev/null
if ! command -v appstreamcli >/dev/null 2>&1; then
    echo "appstreamcli is required to validate AppStream metadata" >&2
    exit 2
fi
appstreamcli validate-tree --no-net "$staging/bundled.AppDir"
# appimagetool validates URLs online by default, so use the offline result above.
"$tool" "$@" --no-appstream "$staging/bundled.AppDir" "$staging/generated.AppImage"
if [ ! -x "$staging/generated.AppImage" ] || ! readelf -h "$staging/generated.AppImage" >/dev/null 2>&1; then
    echo "appimagetool did not produce an executable AppImage" >&2
    exit 1
fi
mv -T --no-clobber "$staging/generated.AppImage" "$output"
if [ -e "$staging/generated.AppImage" ]; then
    echo "AppImage appeared during build: $output" >&2
    exit 2
fi
echo "$output"
