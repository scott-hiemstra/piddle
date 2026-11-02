# Shared by the build-*.sh packaging scripts. Source it from the repo root.
#
# Two packages are produced per format and architecture:
#   piddle      launcher (`piddle`) + terminal UI (`piddle-tui`); no GTK, fine for servers
#   piddle-gui  graphical UI (`piddle-gui`) + desktop entry; recommends piddle
#               (only the launcher uses the GUI, not the other way round, so
#               piddle-gui installs and runs on its own)
set -euo pipefail

# Target architecture: PIDDLE_ARCH (x86_64 or aarch64), default the host's.
ARCH=${PIDDLE_ARCH:-$(uname -m)}
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture: $ARCH (x86_64 or aarch64)" >&2; exit 1 ;;
esac

# The launcher and terminal UI are static musl binaries so they run on any
# Linux of that architecture (old servers included) with no library
# dependencies. The GUI needs GTK anyway, so it links against the system glibc.
MUSL=$ARCH-unknown-linux-musl
CORE_BIN=target/$MUSL/release
GUI_BIN=target/release

# Set PIDDLE_SKIP_BUILD=1 to package existing binaries, e.g. when CI builds
# them in one job and packages them in another.
if [ -z "${PIDDLE_SKIP_BUILD:-}" ]; then
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if [ "$ARCH" != "$(uname -m)" ]; then
    echo "Building $ARCH packages needs an $ARCH machine (the GUI links that architecture's GTK)," >&2
    echo "or prebuilt binaries in $CORE_BIN and $GUI_BIN with PIDDLE_SKIP_BUILD=1." >&2
    exit 1
  fi
  cargo build --release --locked --target "$MUSL" -p piddle -p piddle-tui
  cargo build --release --locked -p piddle-gui
fi
for b in "$CORE_BIN/piddle" "$CORE_BIN/piddle-tui" "$GUI_BIN/piddle-gui"; do
  [ -x "$b" ] || { echo "$b is missing" >&2; exit 1; }
done

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
MAINTAINER="Scott Hiemstra <scott@hiemstra.us>"
HOMEPAGE="https://github.com/scott-hiemstra/piddle"

SUMMARY_CORE="Visual process viewer: terminal interface and launcher"
DESC_CORE="PIDdle shows running processes colored by CPU load (100% = one full core)
with memory, threads, grouping by application or parent, a process tree,
search, and terminate/kill with safety checks. This package provides the
terminal interface (piddle-tui) and the piddle launcher, which starts the
graphical interface when piddle-gui is installed and a display is available."

SUMMARY_GUI="Visual process viewer: graphical GTK 4 interface"
DESC_GUI="The graphical PIDdle interface: processes as bubbles sized by memory and
colored by CPU load, with grouping, a radial process tree, search, details
with CPU history, screenshots, and terminate/kill with safety checks."

# Package contents as "source  destination  mode" (licenses are added per
# format, since Debian and RPM/Arch put them in different places).
CORE_FILES=(
  "$CORE_BIN/piddle                 usr/bin/piddle                                   755"
  "$CORE_BIN/piddle-tui             usr/bin/piddle-tui                               755"
  "README.md                        usr/share/doc/piddle/README.md                   644"
)
GUI_FILES=(
  "$GUI_BIN/piddle-gui              usr/bin/piddle-gui                               755"
  "data/io.github.piddle.desktop    usr/share/applications/io.github.piddle.desktop 644"
)

# stage_files <root> <entries...>: install entries under <root>.
stage_files() {
  local root=$1 src dst mode
  shift
  for f in "$@"; do
    read -r src dst mode <<<"$f"
    install -Dm"$mode" "$src" "$root/$dst"
  done
}
