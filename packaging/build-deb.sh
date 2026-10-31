#!/usr/bin/env bash
# Build Debian packages: target/debian/piddle_<version>_<arch>.deb and
# target/debian/piddle-gui_<version>_<arch>.deb
cd "$(dirname "$0")/.."
. packaging/common.sh

arch=$DEB_ARCH
out=target/debian
mkdir -p "$out"

# Shared-library dependencies of the given binaries, computed by dpkg-shlibdeps.
shlibdeps() {
  local work
  work=$(mktemp -d)
  mkdir -p "$work/debian"
  printf 'Source: piddle\n\nPackage: piddle\nArchitecture: any\n' >"$work/debian/control"
  (cd "$work" && dpkg-shlibdeps -O "$@" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
  rm -rf "$work"
}

# build_deb <package> <summary> <description> <extra depends> <extra fields> <file entries...>
build_deb() {
  local pkg=$1 summary=$2 desc=$3 extra_depends=$4 extra=$5
  shift 5
  local root="$out/${pkg}_${VERSION}_${arch}"
  rm -rf "$root"
  stage_files "$root" "$@"
  install -Dm644 LICENSE "$root/usr/share/doc/$pkg/copyright"

  local bins=()
  for f in "$root"/usr/bin/*; do bins+=("$PWD/$f"); done
  # Static binaries have no library dependencies; join what remains.
  local depends
  depends=$(shlibdeps "${bins[@]}")
  depends=$(printf '%s\n' "$depends" "$extra_depends" | sed '/^$/d' | paste -sd, - | sed 's/,/, /g')

  mkdir -p "$root/DEBIAN"
  {
    echo "Package: $pkg"
    echo "Version: $VERSION"
    echo "Architecture: $arch"
    echo "Maintainer: $MAINTAINER"
    echo "Installed-Size: $(du -sk "$root" | cut -f1)"
    [ -n "$depends" ] && echo "Depends: $depends"
    [ -n "$extra" ] && echo "$extra"
    echo "Section: utils"
    echo "Priority: optional"
    echo "Homepage: $HOMEPAGE"
    echo "Description: $summary"
    sed 's/^/ /' <<<"$desc"
  } >"$root/DEBIAN/control"

  dpkg-deb --root-owner-group --build "$root" "$out/" >/dev/null
  rm -rf "$root"
  echo "$out/${pkg}_${VERSION}_${arch}.deb"
}

build_deb piddle "$SUMMARY_CORE" "$DESC_CORE" "" "" "${CORE_FILES[@]}"
# 0.1.0's piddle package shipped the GUI and its desktop entry.
build_deb piddle-gui "$SUMMARY_GUI" "$DESC_GUI" "piddle (= $VERSION)" \
  "Replaces: piddle (<< 0.2.0)
Breaks: piddle (<< 0.2.0)" \
  "${GUI_FILES[@]}"
