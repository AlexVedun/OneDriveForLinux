#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo metadata --no-deps --offline --format-version 1 --manifest-path "$repo/Cargo.toml" \
    | python3 -c 'import json, sys
packages = json.load(sys.stdin)["packages"]
print(next(package["version"] for package in packages if package["name"] == "onedrive-desktop"))'
