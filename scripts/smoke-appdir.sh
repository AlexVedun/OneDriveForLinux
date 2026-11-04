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
test -s "$appdir/usr/share/icons/hicolor/scalable/apps/io.github.onedrive_linux.svg"
for size in 16 24 32 48 64 128 256 512; do
    cmp "$repo/packaging/icons/hicolor/${size}x${size}/apps/io.github.onedrive_linux.png" \
        "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/io.github.onedrive_linux.png"
done
cmp "$appdir/.DirIcon" "$appdir/usr/share/icons/hicolor/256x256/apps/io.github.onedrive_linux.png"
cmp "$repo/LICENSE" "$appdir/usr/share/licenses/onedrive-linux/LICENSE"
test "$(find "$appdir" -maxdepth 1 -name '*.desktop' | wc -l)" -eq 1
desktop-file-validate "$appdir/io.github.onedrive_linux.desktop"
appstreamcli validate --no-net "$appdir/usr/share/metainfo/io.github.onedrive_linux.appdata.xml"
sh -n "$appdir/AppRun"
sh "$repo/scripts/audit-appdir-libs.sh" "$appdir" > "$scratch/library-audit"
grep -q '^onedrive-daemon:$' "$scratch/library-audit"
grep -q '^onedrive-desktop:$' "$scratch/library-audit"
grep -q '^  HOST libgtk-4.so.1 ' "$scratch/library-audit"
if grep -q '^  MISSING ' "$scratch/library-audit"; then
    echo "AppDir has unresolved libraries" >&2
    exit 1
fi

# A missing dependency must fail the audit, even if ldd still prints other libs.
mkdir -p "$scratch/fake-bin"
cat > "$scratch/fake-bin/ldd" <<'EOF'
#!/bin/sh
printf '\tlibmissing-for-smoke.so.1 => not found\n'
printf '\tlibc.so.6 => /lib/libc.so.6 (0x1234)\n'
EOF
chmod +x "$scratch/fake-bin/ldd"
if PATH="$scratch/fake-bin:$PATH" sh "$repo/scripts/audit-appdir-libs.sh" "$appdir" > "$scratch/missing-audit" 2>&1; then
    echo "Library audit accepted an unresolved dependency" >&2
    exit 1
fi
grep -q '^  MISSING libmissing-for-smoke.so.1$' "$scratch/missing-audit"

if ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appdir.sh" "$appdir" >/dev/null 2>&1; then
    echo "AppDir builder overwrote an existing directory" >&2
    exit 1
fi
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-appdir.*' | wc -l)" -eq 0

# Test launcher process ownership and library paths without opening GTK or D-Bus.
fake="$scratch/fake & AppDir"
mkdir -p "$fake/usr/bin" "$fake/usr/lib" "$fake/usr/lib64" "$fake/usr/lib/x86_64-linux-gnu"
mkdir -p "$fake/usr/share/glib-2.0/schemas"
mkdir -p "$fake/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
printf '"@APPDIR@/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader-svg.so"\n' \
    > "$fake/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/loaders.cache.in"
printf 'compiled fixture\n' > "$fake/usr/share/glib-2.0/schemas/gschemas.compiled"
cp "$appdir/AppRun" "$fake/AppRun"
cat > "$fake/usr/bin/onedrive-daemon" <<'EOF'
#!/bin/sh
trap 'touch "$ONEDRIVE_APPDIR_SMOKE_MARKER"; exit 0' TERM
while :; do sleep 1; done
EOF
cat > "$fake/usr/bin/onedrive-desktop" <<'EOF'
#!/bin/sh
printf '%s\n' "$LD_LIBRARY_PATH" > "$ONEDRIVE_APPDIR_SMOKE_LIBPATH"
printf '%s\n' "$GSETTINGS_SCHEMA_DIR" > "$ONEDRIVE_APPDIR_SMOKE_SCHEMAS"
printf '%s\n' "$XDG_DATA_DIRS" > "$ONEDRIVE_APPDIR_SMOKE_DATA_DIRS"
printf '%s\n' "$GDK_PIXBUF_MODULEDIR" > "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_DIR"
printf '%s\n' "$GDK_PIXBUF_MODULE_FILE" > "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_PATH"
cat "$GDK_PIXBUF_MODULE_FILE" > "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_CACHE"
sleep 1
exit 7
EOF
chmod +x "$fake/usr/bin/onedrive-daemon" "$fake/usr/bin/onedrive-desktop"
export ONEDRIVE_APPDIR_SMOKE_MARKER="$scratch/daemon-stopped"
export ONEDRIVE_APPDIR_SMOKE_LIBPATH="$scratch/library-path"
export ONEDRIVE_APPDIR_SMOKE_SCHEMAS="$scratch/schema-path"
export ONEDRIVE_APPDIR_SMOKE_DATA_DIRS="$scratch/data-dirs"
export ONEDRIVE_APPDIR_SMOKE_PIXBUF_DIR="$scratch/pixbuf-dir"
export ONEDRIVE_APPDIR_SMOKE_PIXBUF_PATH="$scratch/pixbuf-path"
export ONEDRIVE_APPDIR_SMOKE_PIXBUF_CACHE="$scratch/pixbuf-cache"
if LD_LIBRARY_PATH="$scratch/host-lib" GSETTINGS_SCHEMA_DIR="$scratch/host-schemas" \
    XDG_DATA_DIRS="$scratch/host-data" "$fake/AppRun"; then
    echo "AppRun lost the desktop exit status" >&2
    exit 1
else
    status=$?
    test "$status" -eq 7
fi
test -e "$ONEDRIVE_APPDIR_SMOKE_MARKER"
expected_libpath="$fake/usr/lib:$fake/usr/lib64:$fake/usr/lib/x86_64-linux-gnu:$scratch/host-lib"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_LIBPATH")" = "$expected_libpath"
expected_schemas="$fake/usr/share/glib-2.0/schemas:$scratch/host-schemas"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_SCHEMAS")" = "$expected_schemas"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_DATA_DIRS")" = "$fake/usr/share:$scratch/host-data"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_DIR")" = "$fake/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_CACHE")" = "\"$fake/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader-svg.so\""
test ! -e "$(cat "$ONEDRIVE_APPDIR_SMOKE_PIXBUF_PATH")"
