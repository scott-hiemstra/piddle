#!/usr/bin/env bash
# Build against a local piddle-common checkout instead of the pinned commit,
# to change shared code and this app together without tagging or pushing.
#   scripts/use-local-common.sh on [PATH]   (PATH: the checkout; default
#                                            $PIDDLE_COMMON, else ../piddle-common)
#   scripts/use-local-common.sh off
# The work is done by the checkout's own scripts/use-local-common.sh, one
# copy shared by every app (so an older checkout without it needs a pull):
# "on" adds a marked [patch] block to .cargo/config.toml (gitignored) and
# "off" removes it again and re-resolves Cargo.lock against the pinned
# commit; check `git diff Cargo.lock` before committing it. Other settings
# in that file are left alone.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
case "${1:-}" in
  on | off) ;;
  *)
    echo "usage: $0 on [PATH] | off" >&2
    exit 2
    ;;
esac
common=${2:-${PIDDLE_COMMON:-$root/../piddle-common}}
script="$common/scripts/use-local-common.sh"
if [ ! -x "$script" ]; then
  if [ -f "$common/Cargo.toml" ]; then
    echo "error: the piddle-common checkout at $common predates scripts/use-local-common.sh; update it (git pull)" >&2
  else
    echo "error: no piddle-common checkout at $common" >&2
  fi
  echo "Pass its path (on PATH), or set PIDDLE_COMMON. Without one, \"off\" can be done by hand:" >&2
  echo "delete the block between the '# >>> use-local-common.sh' and '# <<< use-local-common.sh'" >&2
  echo "lines in $root/.cargo/config.toml." >&2
  exit 1
fi
exec "$script" "$root" "$1"
