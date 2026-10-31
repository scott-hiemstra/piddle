# Shared by the build-*.sh packaging scripts. Source it from the repo root.
set -euo pipefail
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

cargo build --release -p piddle-gtk

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
MAINTAINER="Scott Hiemstra <scott@hiemstra.us>"
HOMEPAGE="https://github.com/scott-hiemstra/piddle"
SUMMARY="Visual process viewer: bubbles sized by memory, colored by CPU load"
DESCRIPTION="PIDdle shows running processes as bubbles sized by memory and colored by
CPU load (100% = one full core). Group by application or parent, view the
process tree, search, and terminate or kill processes with confirmation."

# The files every package ships, as "source  destination  mode".
FILES=(
  "target/release/piddle            usr/bin/piddle                                   755"
  "data/io.github.piddle.desktop    usr/share/applications/io.github.piddle.desktop 644"
  "README.md                        usr/share/doc/piddle/README.md                   644"
  "LICENSE                          usr/share/licenses/piddle/LICENSE                644"
)

# stage_files <root>: install FILES under <root>.
stage_files() {
  local root=$1 src dst mode
  for f in "${FILES[@]}"; do
    read -r src dst mode <<<"$f"
    install -Dm"$mode" "$src" "$root/$dst"
  done
}
