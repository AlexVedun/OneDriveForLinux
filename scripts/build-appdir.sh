#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
profile=${ONEDRIVE_APPDIR_PROFILE:-release}
case "$profile" in
    dev) binary_profile=debug ;;
    release) binary_profile=release ;;
    *) echo "ONEDRIVE_APPDIR_PROFILE must be dev or release" >&2; exit 2 ;;
esac

output=${1:-"$repo/target/OneDriveForLinux.AppDir"}
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac
if [ -e "$output" ] || [ -L "$output" ]; then
    echo "AppDir already exists: $output" >&2
    exit 2
fi

cd "$repo"
cargo build --profile "$profile" -p onedrive-daemon -p onedrive-desktop
target_dir=${CARGO_TARGET_DIR:-"$repo/target"}
case "$target_dir" in
    /*) ;;
    *) target_dir="$repo/$target_dir" ;;
esac
mkdir -p "$(dirname -- "$output")"
staging=$(mktemp -d "$(dirname -- "$output")/.onedrive-appdir.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
mkdir -p "$staging/usr/bin" "$staging/usr/share/applications" "$staging/usr/share/icons/hicolor/scalable/apps"
install -m 755 "$repo/packaging/AppRun" "$staging/AppRun"
install -m 755 "$target_dir/$binary_profile/onedrive-daemon" "$staging/usr/bin/onedrive-daemon"
install -m 755 "$target_dir/$binary_profile/onedrive-desktop" "$staging/usr/bin/onedrive-desktop"
install -m 644 "$repo/packaging/io.github.onedrive_linux.desktop" "$staging/usr/share/applications/io.github.onedrive_linux.desktop"
install -m 644 "$repo/packaging/io.github.onedrive_linux.svg" "$staging/usr/share/icons/hicolor/scalable/apps/io.github.onedrive_linux.svg"
ln -s ../../AppRun "$staging/usr/bin/onedrive-linux"
ln -s usr/share/applications/io.github.onedrive_linux.desktop "$staging/io.github.onedrive_linux.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/io.github.onedrive_linux.svg "$staging/io.github.onedrive_linux.svg"
ln -s io.github.onedrive_linux.svg "$staging/.DirIcon"
mv -T --no-clobber "$staging" "$output"
if [ -e "$staging" ]; then
    echo "AppDir appeared during build: $output" >&2
    exit 2
fi
echo "$output"
