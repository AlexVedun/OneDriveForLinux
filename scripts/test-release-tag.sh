#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sh "$repo/scripts/app-version.sh")
test "$(sh "$repo/scripts/check-release-tag.sh" "$version")" = "$version"

for invalid in "v$version" "$version-rc1" "$version.1" "01.0.0" "1.00.0" "1.0.00" "1.0" "1.0.0 "; do
    if sh "$repo/scripts/check-release-tag.sh" "$invalid" >/dev/null 2>&1; then
        echo "Invalid release tag was accepted: $invalid" >&2
        exit 1
    fi
done

mismatch=0.0.0
if [ "$version" = "$mismatch" ]; then
    mismatch=0.0.1
fi
if sh "$repo/scripts/check-release-tag.sh" "$mismatch" >/dev/null 2>&1; then
    echo "Tag that differs from Cargo version was accepted: $mismatch" >&2
    exit 1
fi
