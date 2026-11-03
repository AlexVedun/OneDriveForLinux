#!/bin/sh
set -eu

if [ "$#" -ne 1 ] || [ -z "${GITHUB_REPOSITORY:-}" ] || [ -z "${GH_TOKEN:-}" ]; then
    echo "Usage: GITHUB_REPOSITORY=owner/repo GH_TOKEN=token $0 X.Y.Z" >&2
    exit 2
fi

tag=$1
status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
    --header "Authorization: Bearer $GH_TOKEN" \
    --header 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$GITHUB_REPOSITORY/releases/tags/$tag")
case "$status" in
    404) ;;
    200) echo "Release $tag already exists; refusing to move its tag." >&2; exit 1 ;;
    *) echo "Could not verify whether release $tag exists (HTTP $status)." >&2; exit 1 ;;
esac

remote_sha=$(git ls-remote --refs origin "refs/tags/$tag" | cut -f1)
if [ -z "$remote_sha" ]; then
    echo "Tag $tag does not exist on origin." >&2
    exit 1
fi

head_sha=$(git rev-parse HEAD)
if [ "$remote_sha" != "$head_sha" ]; then
    git push --force-with-lease="refs/tags/$tag:$remote_sha" origin "HEAD:refs/tags/$tag"
fi
