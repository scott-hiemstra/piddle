#!/usr/bin/env bash
# Build a Debian package: target/debian/piddle_<version>_<arch>.deb
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

cargo build --release -p piddle-gtk

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(dpkg --print-architecture)
out=target/debian
root=$out/piddle_${version}_${arch}
rm -rf "$root"

install -Dm755 target/release/piddle "$root/usr/bin/piddle"
install -Dm644 data/io.github.piddle.desktop "$root/usr/share/applications/io.github.piddle.desktop"
install -Dm644 README.md "$root/usr/share/doc/piddle/README.md"
install -Dm644 LICENSE "$root/usr/share/doc/piddle/copyright"

# Shared-library dependencies, computed from the binary.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/debian"
printf 'Source: piddle\n\nPackage: piddle\nArchitecture: any\n' > "$work/debian/control"
depends=$(cd "$work" && dpkg-shlibdeps -O "$OLDPWD/$root/usr/bin/piddle" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')

size=$(du -sk "$root" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CTRL
Package: piddle
Version: $version
Architecture: $arch
Maintainer: Scott Hiemstra <scott@hiemstra.us>
Installed-Size: $size
Depends: $depends
Section: utils
Priority: optional
Description: visual process viewer (GTK4)
 PIDdle shows running processes as bubbles sized by memory and colored by
 CPU load (100% = one full core). Group by application or parent, view the
 process tree, search, and terminate or kill processes with confirmation.
CTRL

dpkg-deb --root-owner-group --build "$root" "$out/" >/dev/null
echo "$out/piddle_${version}_${arch}.deb"
