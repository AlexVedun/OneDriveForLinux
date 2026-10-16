#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 APPDIR" >&2
    exit 2
fi

appdir=$(CDPATH= cd -- "$1" && pwd)
for binary in onedrive-daemon onedrive-desktop; do
    path="$appdir/usr/bin/$binary"
    if [ ! -x "$path" ] || ! readelf -h "$path" >/dev/null 2>&1; then
        echo "Missing ELF executable: $path" >&2
        exit 2
    fi
done

# Resolve libraries as AppRun would after a future bundling step. This audit
# reports host dependencies; it does not certify AppImage portability.
lib_paths="$appdir/usr/lib:$appdir/usr/lib64:$appdir/usr/lib/x86_64-linux-gnu"
for binary in onedrive-daemon onedrive-desktop; do
    path="$appdir/usr/bin/$binary"
    echo "$binary:"
    ldd_status=0
    output=$(LD_LIBRARY_PATH="$lib_paths${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ldd "$path" 2>&1) || ldd_status=$?
    if ! printf '%s\n' "$output" | awk -v appdir="$appdir/" '
            /=> not found/ {
                print "  MISSING " $1
                missing = 1
                next
            }
            /=> \// { name = $1; resolved = $3 }
            /^\/[[:graph:]]+[[:space:]]+\(0x/ { name = $1; resolved = $1 }
            resolved != "" {
                if (index(resolved, appdir) == 1) {
                    print "  BUNDLED " name " " resolved
                } else {
                    print "  HOST " name " " resolved
                }
                resolved = ""
            }
            END { if (missing) exit 1 }
        '; then
        echo "Unresolved dependencies in $binary" >&2
        exit 1
    fi
    if [ "$ldd_status" -ne 0 ]; then
        printf '%s\n' "$output" >&2
        echo "ldd failed for $binary" >&2
        exit 1
    fi
done
