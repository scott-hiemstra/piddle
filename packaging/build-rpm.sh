#!/usr/bin/env bash
# Build an RPM: target/rpm/piddle-<version>-1.<arch>.rpm
cd "$(dirname "$0")/.."
. packaging/common.sh

top=$PWD/target/rpm/build
rm -rf "$top"
mkdir -p "$top"/{SPECS,BUILDROOT,RPMS}
stage=$top/stage
stage_files "$stage"

cat > "$top/SPECS/piddle.spec" <<SPEC
%global debug_package %{nil}
%global _build_id_links none

Name:           piddle
Version:        $VERSION
Release:        1
Summary:        $SUMMARY
License:        MIT
URL:            $HOMEPAGE
Packager:       $MAINTAINER

%description
$DESCRIPTION

Requires GTK 4.14 or newer.

%install
cp -a $stage/. %{buildroot}/

%files
/usr/bin/piddle
/usr/share/applications/io.github.piddle.desktop
%doc /usr/share/doc/piddle/README.md
%license /usr/share/licenses/piddle/LICENSE
SPEC

# Library requirements (libgtk-4.so.1 etc.) are generated from the binary.
rpmbuild --quiet -bb --define "_topdir $top" --define "_dbpath $top/rpmdb" "$top/SPECS/piddle.spec"
out=$(find "$top/RPMS" -name '*.rpm' | head -1)
mv "$out" target/rpm/
echo "target/rpm/$(basename "$out")"
