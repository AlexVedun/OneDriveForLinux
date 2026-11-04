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

svg_loader_dir=$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0)
svg_loader="$svg_loader_dir/libpixbufloader-svg.so"
query_loaders=$(pkg-config --variable=gdk_pixbuf_query_loaders gdk-pixbuf-2.0)
if [ ! -f "$svg_loader" ] || [ ! -x "$query_loaders" ]; then
    echo "SVG icon loader is unavailable; install the GdkPixbuf SVG loader and query tool" >&2
    exit 2
fi

mkdir -p "$(dirname -- "$output")"
staging=$(mktemp -d "$(dirname -- "$output")/.onedrive-bundle.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
cp -a "$input/." "$staging/"
bundled_loader_dir="$staging/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
mkdir -p "$bundled_loader_dir"
bundled_loader="$bundled_loader_dir/libpixbufloader-svg.so"
install -m 755 "$svg_loader" "$bundled_loader"
"$tool" "$@" --appdir="$staging" \
    --deploy-deps-only="$staging/usr/bin/onedrive-daemon" \
    --deploy-deps-only="$staging/usr/bin/onedrive-desktop" \
    --deploy-deps-only="$bundled_loader" \
    --custom-apprun="$repo/packaging/AppRun"

audit=$(sh "$repo/scripts/audit-appdir-libs.sh" "$staging")
require_audit() {
    if ! printf '%s\n' "$1" | grep -q "$2"; then
        echo "Missing expected AppDir dependency: $2" >&2
        printf '%s\n' "$1" >&2
        exit 1
    fi
}
require_audit "$audit" '^  BUNDLED libgtk-4.so.1 '
require_audit "$audit" '^  BUNDLED libadwaita-1.so.0 '
require_audit "$audit" '^  BUNDLED libgio-2.0.so.0 '
require_audit "$audit" '^  BUNDLED libglib-2.0.so.0 '
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

icon_theme=/usr/share/icons/Adwaita
if [ ! -f "$icon_theme/index.theme" ]; then
    echo "Adwaita icon theme is unavailable: $icon_theme" >&2
    exit 2
fi
mkdir -p "$staging/usr/share/icons"
cp -a "$icon_theme" "$staging/usr/share/icons/"

"$query_loaders" "$bundled_loader" \
    | python3 -c 'import sys; print(sys.stdin.read().replace(sys.argv[1], "@APPDIR@"), end="")' "$staging" \
    > "$bundled_loader_dir/loaders.cache.in"
if ! grep -q '@APPDIR@/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader-svg.so' \
    "$bundled_loader_dir/loaders.cache.in"; then
    echo "SVG loader cache does not point into the AppDir" >&2
    exit 1
fi
plugin_audit=$(sh "$repo/scripts/audit-appdir-libs.sh" "$staging")
require_audit "$plugin_audit" '^svg-loader:$'
require_audit "$plugin_audit" '^  BUNDLED librsvg-2.so.2 '

python3 "$repo/scripts/collect-appdir-licenses.py" "$input" "$staging" "$svg_loader"

mv -T --no-clobber "$staging" "$output"
if [ -e "$staging" ]; then
    echo "AppDir appeared during bundling: $output" >&2
    exit 2
fi
echo "$output"
