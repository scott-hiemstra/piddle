#!/usr/bin/env bash
# Build Arch Linux packages: target/arch/piddle-<version>-1-<arch>.pkg.tar.zst
# and target/arch/piddle-gui-<version>-1-<arch>.pkg.tar.zst
cd "$(dirname "$0")/.."
. packaging/common.sh

work=$PWD/target/arch/build
rm -rf "$work"
mkdir -p "$work"
sources=(piddle piddle-tui piddle-gui io.github.piddle.desktop README.md LICENSE)
cp "$CORE_BIN/piddle" "$CORE_BIN/piddle-tui" "$GUI_BIN/piddle-gui" \
  data/io.github.piddle.desktop README.md LICENSE "$work/"
sums=$(cd "$work" && sha256sum "${sources[@]}" | awk '{printf "'"'"'%s'"'"' ", $1}')

cat >"$work/PKGBUILD" <<PKG
# Maintainer: $MAINTAINER
pkgbase=piddle
pkgname=('piddle' 'piddle-gui')
pkgver=$VERSION
pkgrel=1
arch=('$ARCH')
url="$HOMEPAGE"
license=('MIT')
options=('!strip' '!debug')
source=(${sources[*]})
sha256sums=($sums)

package_piddle() {
  pkgdesc="$SUMMARY_CORE"
  # Static binaries: no runtime dependencies.
  optdepends=('piddle-gui: graphical interface')
  install -Dm755 piddle "\$pkgdir/usr/bin/piddle"
  install -Dm755 piddle-tui "\$pkgdir/usr/bin/piddle-tui"
  install -Dm644 README.md "\$pkgdir/usr/share/doc/piddle/README.md"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/piddle/LICENSE"
}

package_piddle-gui() {
  pkgdesc="$SUMMARY_GUI"
  depends=("piddle=\$pkgver" 'gtk4>=4.14' 'glib2' 'cairo' 'pango' 'graphene' 'glibc' 'gcc-libs')
  install -Dm755 piddle-gui "\$pkgdir/usr/bin/piddle-gui"
  install -Dm644 io.github.piddle.desktop "\$pkgdir/usr/share/applications/io.github.piddle.desktop"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/piddle-gui/LICENSE"
}
PKG

# --nodeps and PACMAN=true: this may run on a non-Arch host with no pacman
# database; dependencies are declared above and resolved at install time.
# makepkg is chatty (and Debian's fakeroot can print a harmless race message),
# so keep its log and only show it if the build fails.
if ! (cd "$work" && CARCH=$ARCH PKGEXT=.pkg.tar.zst PACKAGER="$MAINTAINER" PACMAN=true \
  makepkg --force --nodeps --noconfirm >makepkg.log 2>&1); then
  cat "$work/makepkg.log" >&2
  exit 1
fi
for f in "$work"/piddle-*.pkg.tar.zst; do
  mv "$f" target/arch/
  echo "target/arch/$(basename "$f")"
done
