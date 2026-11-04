#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$scratch/input.AppDir/usr/bin" "$scratch/loaders" "$scratch/bin"
cp /bin/true "$scratch/input.AppDir/usr/bin/onedrive-daemon"
cp /bin/true "$scratch/input.AppDir/usr/bin/onedrive-desktop"
cp /bin/true "$scratch/loaders/libpixbufloader-svg.so"

cat > "$scratch/bin/pkg-config" <<'EOF'
#!/bin/sh
case "$1" in
    --variable=gdk_pixbuf_moduledir) printf '%s\n' "$TEST_SVG_LOADER_DIR" ;;
    --variable=gdk_pixbuf_query_loaders) printf '%s\n' /bin/true ;;
    *) exit 2 ;;
esac
EOF
cat > "$scratch/mock-linuxdeploy" <<'EOF'
#!/bin/sh
set -eu
appdir=
loader=
for argument do
    case "$argument" in
        --appdir=*) appdir=${argument#--appdir=} ;;
        --deploy-deps-only=*/libpixbufloader-svg.so) loader=${argument#--deploy-deps-only=} ;;
    esac
done
test -n "$appdir"
test "$loader" = "$appdir/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader-svg.so"
cmp "$TEST_SVG_LOADER_DIR/libpixbufloader-svg.so" "$loader"
touch "$TEST_SVG_STAGING_MARKER"
exit 41
EOF
chmod +x "$scratch/bin/pkg-config" "$scratch/mock-linuxdeploy"

if TEST_SVG_LOADER_DIR="$scratch/loaders" TEST_SVG_STAGING_MARKER="$scratch/checked" \
    PATH="$scratch/bin:$PATH" LINUXDEPLOY="$scratch/mock-linuxdeploy" \
    sh "$repo/scripts/bundle-appdir-libs.sh" "$scratch/input.AppDir" "$scratch/output.AppDir" \
    >"$scratch/bundle.log" 2>&1; then
    echo "Bundler accepted a failed linuxdeploy run" >&2
    exit 1
fi
test -f "$scratch/checked"
test ! -e "$scratch/output.AppDir"
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-bundle.*' | wc -l)" -eq 0
