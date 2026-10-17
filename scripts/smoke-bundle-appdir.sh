#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

ONEDRIVE_APPDIR_PROFILE=dev sh "$repo/scripts/build-appdir.sh" "$scratch/input.AppDir" >/dev/null
if sh "$repo/scripts/bundle-appdir-libs.sh" "$scratch/input.AppDir" "$scratch/input.AppDir/../input.AppDir/nested.AppDir" >/dev/null 2>&1; then
    echo "Bundler accepted output inside its input" >&2
    exit 1
fi
test ! -e "$scratch/input.AppDir/nested.AppDir"
if LINUXDEPLOY=false sh "$repo/scripts/bundle-appdir-libs.sh" "$scratch/input.AppDir" "$scratch/failed.AppDir" >/dev/null 2>&1; then
    echo "Bundler accepted a failed linuxdeploy run" >&2
    exit 1
fi
test ! -e "$scratch/failed.AppDir"
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-bundle.*' | wc -l)" -eq 0

sh "$repo/scripts/bundle-appdir-libs.sh" "$scratch/input.AppDir" "$scratch/bundled.AppDir" > "$scratch/bundle.log"
bundled="$scratch/bundled.AppDir"
test -x "$bundled/usr/bin/onedrive-desktop"
cmp "$repo/LICENSE" "$bundled/usr/share/licenses/onedrive-linux/LICENSE"
python3 - "$bundled" <<'PY'
import json
from pathlib import Path
import sys

appdir = Path(sys.argv[1])
licenses = appdir / "usr/share/licenses/onedrive-linux"
manifest = json.loads((licenses / "native-libraries.json").read_text())
bundled = {path.name for path in (appdir / "usr/lib").iterdir()
           if path.is_file() and ".so" in path.name}
assert {item["library"] for item in manifest["libraries"]} == bundled
packages = {(item["manager"], item["package"]): item for item in manifest["packages"]}
libraries = {item["library"]: item for item in manifest["libraries"]}
for library in ("libgtk-4.so.1", "libadwaita-1.so.0", "libglib-2.0.so.0"):
    owner = libraries[library]
    assert packages[(owner["manager"], owner["package"])]["license_files"], library
assert (appdir / "usr/share/icons/Adwaita/index.theme").is_file()
theme = next(item for item in manifest["resources"]
             if item["resource"] == "usr/share/icons/Adwaita")
assert packages[(theme["manager"], theme["package"])]["license_files"]
for package in manifest["packages"]:
    for name in package["license_files"]:
        assert (appdir / name).is_file(), name
PY
desktop-file-validate "$bundled/io.github.onedrive_linux.desktop"
schema_dir="$bundled/usr/share/glib-2.0/schemas"
test -s "$schema_dir/gschemas.compiled"
test -f "$schema_dir/org.gtk.gtk4.Settings.FileChooser.gschema.xml"
glib-compile-schemas --strict --dry-run "$schema_dir"
mkdir -p "$scratch/empty-data"
XDG_DATA_HOME="$scratch/empty-data" XDG_DATA_DIRS="$scratch/empty-data" \
    GSETTINGS_SCHEMA_DIR="$schema_dir" gsettings list-schemas \
    | grep -q '^org.gtk.gtk4.Settings.FileChooser$'
sh "$repo/scripts/audit-appdir-libs.sh" "$bundled" > "$scratch/audit.log"
grep -q '^  BUNDLED libgtk-4.so.1 ' "$scratch/audit.log"
grep -q '^  BUNDLED libadwaita-1.so.0 ' "$scratch/audit.log"
sh "$repo/scripts/audit-appdir-libs.sh" "$scratch/input.AppDir" > "$scratch/input-audit.log"
grep -q '^  HOST libgtk-4.so.1 ' "$scratch/input-audit.log"
if sh "$repo/scripts/bundle-appdir-libs.sh" "$scratch/input.AppDir" "$bundled" >/dev/null 2>&1; then
    echo "Bundler overwrote an existing AppDir" >&2
    exit 1
fi
test "$(find "$scratch" -maxdepth 1 -name '.onedrive-bundle.*' | wc -l)" -eq 0
