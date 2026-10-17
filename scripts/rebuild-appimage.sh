#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tools_dir=${ONEDRIVE_APPIMAGE_TOOLS_DIR:-"$repo/target/appimage-tools"}
case "$tools_dir" in
    /*) ;;
    *) tools_dir="$repo/$tools_dir" ;;
esac

if [ -z "${LINUXDEPLOY:-}" ] || [ -z "${APPIMAGETOOL:-}" ] || [ -z "${APPIMAGETOOL_RUNTIME_FILE:-}" ]; then
    if [ ! -d "$tools_dir" ]; then
        mkdir -p "$(dirname -- "$tools_dir")"
        sh "$repo/scripts/fetch-appimage-tools.sh" "$tools_dir"
    fi
    LINUXDEPLOY=${LINUXDEPLOY:-"$tools_dir/linuxdeploy-x86_64.AppImage"}
    APPIMAGETOOL=${APPIMAGETOOL:-"$tools_dir/appimagetool-x86_64.AppImage"}
    APPIMAGETOOL_RUNTIME_FILE=${APPIMAGETOOL_RUNTIME_FILE:-"$tools_dir/runtime-x86_64"}
fi
export LINUXDEPLOY APPIMAGETOOL APPIMAGETOOL_RUNTIME_FILE

version=$(sh "$repo/scripts/app-version.sh")
name="OneDriveForLinux-$version-x86_64.AppImage"
dist="$repo/target/dist"
mkdir -p "$dist"
staging=$(mktemp -d "$dist/.appimage-rebuild.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM

sh "$repo/scripts/build-appimage.sh" "$staging/$name"
mv -T -- "$staging/$name" "$dist/$name"
echo "Готово: $dist/$name"
