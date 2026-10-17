#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 OUTPUT_DIRECTORY" >&2
    exit 2
fi
case "$1" in
    /*) output=$1 ;;
    *) output="$PWD/$1" ;;
esac
output=$(realpath -m -- "$output")
if [ -e "$output" ] || [ -L "$output" ]; then
    echo "Tool directory already exists: $output" >&2
    exit 2
fi
mkdir -p "$(dirname -- "$output")"
staging=$(mktemp -d "$(dirname -- "$output")/.onedrive-tools.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM

# The linuxdeploy continuous asset is mutable. Its checksum pins the build
# tested here; update it deliberately when moving to another upstream build.
curl --fail --location --retry 3 --silent --show-error \
    --output "$staging/linuxdeploy-x86_64.AppImage" \
    https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
curl --fail --location --retry 3 --silent --show-error \
    --output "$staging/appimagetool-x86_64.AppImage" \
    https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
curl --fail --location --retry 3 --silent --show-error \
    --output "$staging/runtime-x86_64" \
    https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
(
    cd "$staging"
    printf '%s\n' \
        '8aea8da0f7f7039d2a2cecb14657d752a222a5e1d3825caeef186c82f751cdd1  linuxdeploy-x86_64.AppImage' \
        'ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0  appimagetool-x86_64.AppImage' \
        '2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d  runtime-x86_64' \
        | sha256sum -c -
)
chmod +x "$staging/linuxdeploy-x86_64.AppImage" "$staging/appimagetool-x86_64.AppImage"
mv -T --no-clobber "$staging" "$output"
if [ -e "$staging" ]; then
    echo "Tool directory appeared during download: $output" >&2
    exit 2
fi
echo "$output"
