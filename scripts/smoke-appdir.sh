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
cmp "$repo/LICENSE" "$appdir/usr/share/licenses/onedrive-linux/LICENSE"
test "$(find "$appdir" -maxdepth 1 -name '*.desktop' | wc -l)" -eq 1
desktop-file-validate "$appdir/io.github.onedrive_linux.desktop"
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
fake="$scratch/fake AppDir"
mkdir -p "$fake/usr/bin" "$fake/usr/lib" "$fake/usr/lib64" "$fake/usr/lib/x86_64-linux-gnu"
cp "$appdir/AppRun" "$fake/AppRun"
cat > "$fake/usr/bin/onedrive-daemon" <<'EOF'
#!/bin/sh
trap 'touch "$ONEDRIVE_APPDIR_SMOKE_MARKER"; exit 0' TERM
while :; do sleep 1; done
EOF
cat > "$fake/usr/bin/onedrive-desktop" <<'EOF'
#!/bin/sh
printf '%s\n' "$LD_LIBRARY_PATH" > "$ONEDRIVE_APPDIR_SMOKE_LIBPATH"
sleep 1
exit 7
EOF
chmod +x "$fake/usr/bin/onedrive-daemon" "$fake/usr/bin/onedrive-desktop"
export ONEDRIVE_APPDIR_SMOKE_MARKER="$scratch/daemon-stopped"
export ONEDRIVE_APPDIR_SMOKE_LIBPATH="$scratch/library-path"
if LD_LIBRARY_PATH="$scratch/host-lib" "$fake/AppRun"; then
    echo "AppRun lost the desktop exit status" >&2
    exit 1
else
    status=$?
    test "$status" -eq 7
fi
test -e "$ONEDRIVE_APPDIR_SMOKE_MARKER"
expected_libpath="$fake/usr/lib:$fake/usr/lib64:$fake/usr/lib/x86_64-linux-gnu:$scratch/host-lib"
test "$(cat "$ONEDRIVE_APPDIR_SMOKE_LIBPATH")" = "$expected_libpath"
