#!/usr/bin/env bash
# Pin every piddle-common dependency to the commit of a release tag.
#   scripts/bump-common.sh v0.2.0
# Rewrites the rev = "..." (and the # vX.Y.Z comment) of each piddle-common
# entry in Cargo.toml's [workspace.dependencies], then updates Cargo.lock.
set -euo pipefail
tag=${1:?usage: bump-common.sh vX.Y.Z}
url=https://github.com/scott-hiemstra/piddle-common
root=$(cd "$(dirname "$0")/.." && pwd)
# An annotated tag's ^{} line is the commit it points at; prefer it.
refs=$(git ls-remote "$url" "refs/tags/$tag" "refs/tags/$tag^{}")
rev=$(printf '%s\n' "$refs" | awk '/\^\{\}$/ { print $1; found = 1 } END { if (!found) exit 1 }') ||
  rev=$(printf '%s\n' "$refs" | awk 'NR == 1 { print $1 }')
if [ -z "$rev" ]; then
  echo "error: $url has no tag $tag" >&2
  exit 1
fi
sed -i -E "s|^(piddle-[a-z]+ = \{ git = \"$url\", rev = \")[0-9a-f]+(\" \}).*|\1$rev\2 # $tag|" "$root/Cargo.toml"
grep -F "$url" "$root/Cargo.toml"
(cd "$root" && cargo update -p piddle-encode)
