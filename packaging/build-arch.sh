#!/usr/bin/env bash
# Build an Arch Linux package: target/arch/piddle-<version>-1-<arch>.pkg.tar.zst
cd "$(dirname "$0")/.."
. packaging/common.sh

work=$PWD/target/arch/build
rm -rf "$work"
mkdir -p "$work"
for f in "${FILES[@]}"; do
  read -r src _ _ <<<"$f"
  cp "$src" "$work/"
done

cat > "$work/PKGBUILD" <<PKG
# Maintainer: $MAINTAINER
pkgname=piddle
pkgver=$VERSION
pkgrel=1
pkgdesc="$SUMMARY"
arch=('x86_64')
url="$HOMEPAGE"
license=('MIT')
depends=('gtk4>=4.14' 'glib2' 'cairo' 'pango' 'graphene' 'glibc' 'gcc-libs')
options=('!strip' '!debug')
source=('piddle' 'io.github.piddle.desktop' 'README.md' 'LICENSE')
sha256sums=($(cd "$work" && sha256sum piddle io.github.piddle.desktop README.md LICENSE | awk '{printf "'"'"'%s'"'"' ", $1}'))

package() {
  install -Dm755 piddle "\$pkgdir/usr/bin/piddle"
  install -Dm644 io.github.piddle.desktop "\$pkgdir/usr/share/applications/io.github.piddle.desktop"
  install -Dm644 README.md "\$pkgdir/usr/share/doc/piddle/README.md"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/piddle/LICENSE"
}
PKG

# --nodeps and PACMAN=true: this may run on a non-Arch host with no pacman
# database; dependencies are declared above and resolved at install time.
# makepkg is chatty (and Debian's fakeroot can print a harmless race message),
# so keep its log and only show it if the build fails.
if ! (cd "$work" && CARCH=x86_64 PKGEXT=.pkg.tar.zst PACKAGER="$MAINTAINER" PACMAN=true \
  makepkg --force --nodeps --noconfirm >makepkg.log 2>&1); then
  cat "$work/makepkg.log" >&2
  exit 1
fi
out=$(find "$work" -maxdepth 1 -name 'piddle-*.pkg.tar.zst' | head -1)
mv "$out" target/arch/
echo "target/arch/$(basename "$out")"
