#!/usr/bin/env bash
# Build against a local piddle-common checkout instead of the pinned commit,
# to change shared code and this app together without tagging or pushing.
#   scripts/use-local-common.sh on [PATH]   (default ../piddle-common)
#   scripts/use-local-common.sh off
# "on" adds a marked [patch] block for the piddle-common crates to
# .cargo/config.toml (gitignored); "off" removes that block. Anything else in
# the file is left alone, and the file is only deleted if nothing else is in
# it. Cargo.lock changes while it's on; "off" restores it, so run "off"
# before committing a lockfile. Cargo still fetches the pinned commit (it
# must be on GitHub), but builds from PATH.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
config="$root/.cargo/config.toml"
begin="# >>> use-local-common.sh (remove with: scripts/use-local-common.sh off)"
end="# <<< use-local-common.sh"
table='[patch."https://github.com/scott-hiemstra/piddle-common"]'

# The config without our block (empty if there is no config).
others() {
  [ -f "$config" ] || return 0
  # Written whole by an older version of this script: all of it is ours.
  if head -1 "$config" | grep -q '^# Written by scripts/use-local-common.sh'; then
    return 0
  fi
  awk -v b="$begin" -v e="$end" '$0 == b { skip = 1; next } $0 == e { skip = 0; next } !skip' "$config"
}

# Write $1 as the config, or remove the file (and an empty .cargo) if $1 has
# nothing but blank lines.
write_config() {
  if [ -z "$(printf '%s' "$1" | tr -d '[:space:]')" ]; then
    rm -f "$config"
    rmdir "$root/.cargo" 2>/dev/null || true
  else
    printf '%s\n' "$1" >"$config"
  fi
}

case "${1:-}" in
  on)
    common=$(cd "${2:-$root/../piddle-common}" && pwd)
    rest=$(others)
    # A second table with the same name would make the file invalid TOML.
    if grep -qF "$table" <<<"$rest"; then
      echo "error: $config already has its own $table table; merge the paths into it by hand" >&2
      exit 1
    fi
    block=$(
      echo "$begin"
      echo "$table"
      for c in "$common"/crates/*/; do
        c=$(basename "$c")
        echo "$c = { path = \"$common/crates/$c\" }"
      done
      echo "$end"
    )
    mkdir -p "$root/.cargo"
    if [ -n "$(printf '%s' "$rest" | tr -d '[:space:]')" ]; then
      write_config "$rest"$'\n\n'"$block"
    else
      write_config "$block"
    fi
    echo "building against $common"
    ;;
  off)
    write_config "$(others)"
    # While patched, Cargo.lock lists the crates without their git source;
    # resolve once more so it points at the pinned commit again.
    (cd "$root" && cargo metadata --format-version 1 >/dev/null)
    echo "building against the pinned piddle-common commit"
    ;;
  *)
    echo "usage: $0 on [PATH] | off" >&2
    exit 2
    ;;
esac
