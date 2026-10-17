#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "Usage: LINUXDEPLOY=/path/to/linuxdeploy $0 INPUT_APPDIR OUTPUT_APPDIR" >&2
    exit 2
fi

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
input=$(CDPATH= cd -- "$1" && pwd -P)
output=$2
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac
output=$(realpath -m -- "$output")
case "$output" in
    "$input"|"$input"/*) echo "Output must be outside the input AppDir" >&2; exit 2 ;;
esac
if [ -e "$output" ] || [ -L "$output" ]; then
    echo "AppDir already exists: $output" >&2
    exit 2
fi
for binary in onedrive-daemon onedrive-desktop; do
    if [ ! -x "$input/usr/bin/$binary" ]; then
        echo "Missing executable: $input/usr/bin/$binary" >&2
        exit 2
    fi
done

tool=${LINUXDEPLOY:-linuxdeploy}
if ! command -v "$tool" >/dev/null 2>&1; then
    echo "linuxdeploy is unavailable; set LINUXDEPLOY to its executable path" >&2
    exit 2
fi
case "$tool" in
    *.AppImage) set -- --appimage-extract-and-run ;;
    *) set -- ;;
esac

mkdir -p "$(dirname -- "$output")"
staging=$(mktemp -d "$(dirname -- "$output")/.onedrive-bundle.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
cp -a "$input/." "$staging/"
"$tool" "$@" --appdir="$staging" \
    --deploy-deps-only="$staging/usr/bin/onedrive-daemon" \
    --deploy-deps-only="$staging/usr/bin/onedrive-desktop" \
    --custom-apprun="$repo/packaging/AppRun"

audit=$(sh "$repo/scripts/audit-appdir-libs.sh" "$staging")
printf '%s\n' "$audit" | grep -q '^  BUNDLED libgtk-4.so.1 '
printf '%s\n' "$audit" | grep -q '^  BUNDLED libadwaita-1.so.0 '
printf '%s\n' "$audit" | grep -q '^  BUNDLED libgio-2.0.so.0 '
printf '%s\n' "$audit" | grep -q '^  BUNDLED libglib-2.0.so.0 '
cmp "$repo/packaging/AppRun" "$staging/AppRun"

schema_source=$(pkg-config --variable=schemasdir gio-2.0)
if [ ! -d "$schema_source" ]; then
    echo "GSettings schema directory is unavailable: $schema_source" >&2
    exit 2
fi
set -- "$schema_source"/org.gtk.gtk4.*.gschema.xml
if [ ! -f "$1" ]; then
    echo "GTK 4 GSettings schemas are unavailable in $schema_source" >&2
    exit 2
fi
schema_target="$staging/usr/share/glib-2.0/schemas"
mkdir -p "$schema_target"
install -m 644 "$@" "$schema_target/"
glib-compile-schemas --strict "$schema_target"
test -s "$schema_target/gschemas.compiled"

python3 "$repo/scripts/collect-appdir-licenses.py" "$input" "$staging"

mv -T --no-clobber "$staging" "$output"
if [ -e "$staging" ]; then
    echo "AppDir appeared during bundling: $output" >&2
    exit 2
fi
echo "$output"
