#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

git init --bare -q "$tmp/origin.git"
git init -q "$tmp/work"
git -C "$tmp/work" config user.name 'Release test'
git -C "$tmp/work" config user.email 'release-test@example.invalid'
git -C "$tmp/work" remote add origin "$tmp/origin.git"
printf 'old\n' > "$tmp/work/file"
git -C "$tmp/work" add file
git -C "$tmp/work" commit -qm old
git -C "$tmp/work" tag 1.0.1
git -C "$tmp/work" push -q origin refs/tags/1.0.1
old_sha=$(git -C "$tmp/work" rev-parse HEAD)
printf 'fixed\n' > "$tmp/work/file"
git -C "$tmp/work" commit -qam fixed
new_sha=$(git -C "$tmp/work" rev-parse HEAD)

mkdir "$tmp/bin"
cat > "$tmp/bin/curl" <<'EOF'
#!/bin/sh
printf '%s' "$MOCK_HTTP_STATUS"
EOF
chmod +x "$tmp/bin/curl"

if (cd "$tmp/work" && PATH="$tmp/bin:$PATH" MOCK_HTTP_STATUS=200 GITHUB_REPOSITORY=owner/repo GH_TOKEN=test \
    sh "$repo/scripts/retarget-release-tag.sh" 1.0.1 >/dev/null 2>&1); then
    echo 'Existing release was accepted.' >&2
    exit 1
fi
test "$(git -C "$tmp/work" ls-remote --refs origin refs/tags/1.0.1 | cut -f1)" = "$old_sha"

if (cd "$tmp/work" && PATH="$tmp/bin:$PATH" MOCK_HTTP_STATUS=503 GITHUB_REPOSITORY=owner/repo GH_TOKEN=test \
    sh "$repo/scripts/retarget-release-tag.sh" 1.0.1 >/dev/null 2>&1); then
    echo 'Uncertain release lookup was accepted.' >&2
    exit 1
fi
test "$(git -C "$tmp/work" ls-remote --refs origin refs/tags/1.0.1 | cut -f1)" = "$old_sha"

(cd "$tmp/work" && PATH="$tmp/bin:$PATH" MOCK_HTTP_STATUS=404 GITHUB_REPOSITORY=owner/repo GH_TOKEN=test \
    sh "$repo/scripts/retarget-release-tag.sh" 1.0.1 >/dev/null)
test "$(git -C "$tmp/work" ls-remote --refs origin refs/tags/1.0.1 | cut -f1)" = "$new_sha"
