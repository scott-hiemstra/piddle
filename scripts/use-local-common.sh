#!/usr/bin/env bash
# Build against a local piddle-common checkout instead of the pinned commit,
# to change shared code and this app together without tagging or pushing.
#   scripts/use-local-common.sh on [PATH]   (default ../piddle-common)
#   scripts/use-local-common.sh off
# "on" writes a .cargo/config.toml (gitignored) with a [patch] for the
# piddle-common crates; "off" removes it. Cargo.lock changes while it's on,
# so run "off" before committing a lockfile. Cargo still fetches the pinned
# commit (it must be on GitHub), but builds from PATH.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
config="$root/.cargo/config.toml"
case "${1:-}" in
  on)
    common=$(cd "${2:-$root/../piddle-common}" && pwd)
    mkdir -p "$root/.cargo"
    {
      echo "# Written by scripts/use-local-common.sh; remove with: scripts/use-local-common.sh off"
      echo '[patch."https://github.com/scott-hiemstra/piddle-common"]'
      for c in "$common"/crates/*/; do
        c=$(basename "$c")
        echo "$c = { path = \"$common/crates/$c\" }"
      done
    } > "$config"
    echo "building against $common"
    ;;
  off)
    rm -f "$config"
    rmdir "$root/.cargo" 2>/dev/null || true
    echo "building against the pinned piddle-common commit"
    ;;
  *)
    echo "usage: $0 on [PATH] | off" >&2
    exit 2
    ;;
esac
