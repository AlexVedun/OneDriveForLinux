#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 X.Y.Z" >&2
    exit 2
fi

tag_value=$1
if ! printf '%s\n' "$tag_value" | LC_ALL=C grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
    echo "Release tag must have the exact X.Y.Z format: $tag_value" >&2
    exit 2
fi

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_version=$(sh "$repo/scripts/app-version.sh")
if [ "$tag_value" != "$app_version" ]; then
    echo "Release tag $tag_value does not match Cargo version $app_version" >&2
    exit 2
fi

printf '%s\n' "$tag_value"
