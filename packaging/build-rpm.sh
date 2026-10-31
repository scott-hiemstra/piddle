#!/usr/bin/env bash
# Build RPMs: target/rpm/piddle-<version>-1.<arch>.rpm and
# target/rpm/piddle-gui-<version>-1.<arch>.rpm
cd "$(dirname "$0")/.."
. packaging/common.sh

top=$PWD/target/rpm/build
rm -rf "$top"
mkdir -p "$top"/{SPECS,RPMS}
stage=$top/stage
stage_files "$stage" "${CORE_FILES[@]}" "${GUI_FILES[@]}"
install -Dm644 LICENSE "$stage/usr/share/licenses/piddle/LICENSE"

cat >"$top/SPECS/piddle.spec" <<SPEC
%global debug_package %{nil}
%global _build_id_links none

Name:           piddle
Version:        $VERSION
Release:        1
Summary:        $SUMMARY_CORE
License:        MIT
URL:            $HOMEPAGE
Packager:       $MAINTAINER

%description
$DESC_CORE

%package gui
Summary:        $SUMMARY_GUI
Requires:       piddle = %{version}-%{release}
# 0.1.0's piddle package was the GUI; keep it installed across the upgrade.
Obsoletes:      piddle < 0.2.0

%description gui
$DESC_GUI

Requires GTK 4.14 or newer.

%install
cp -a $stage/. %{buildroot}/

%files
/usr/bin/piddle
/usr/bin/piddle-tui
%doc /usr/share/doc/piddle/README.md
%license /usr/share/licenses/piddle/LICENSE

%files gui
/usr/bin/piddle-gui
/usr/share/applications/io.github.piddle.desktop
%license /usr/share/licenses/piddle/LICENSE
SPEC

# Library requirements (libgtk-4.so.1 etc.) are generated from each binary.
rpmbuild --quiet -bb --define "_topdir $top" --define "_dbpath $top/rpmdb" "$top/SPECS/piddle.spec"
for f in $(find "$top/RPMS" -name '*.rpm'); do
  mv "$f" target/rpm/
  echo "target/rpm/$(basename "$f")"
done
