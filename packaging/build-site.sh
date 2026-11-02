#!/usr/bin/env bash
# Build the package repository published on GitHub Pages from one release's
# packages: an apt repository (deb/), a dnf repository (rpm/), the public
# key and an index page with the setup instructions. Arch (arch/) is added
# by build-site-arch.sh, which needs repo-add.
#
#   packaging/build-site.sh <dir with the release's .deb and .rpm files> <site dir>
#
# Signs with the secret key in the GnuPG home, which must be the one in
# packaging/signing-key.asc. SITE_URL overrides the published address (for
# testing against a local server). Needs apt-ftparchive (apt-utils),
# rpmsign (rpm) and createrepo_c.
set -euo pipefail
cd "$(dirname "$0")/.."
. packaging/site-common.sh

in=$(realpath "$1")
site=$(realpath -m "$2")
mkdir -p "$site"
cp packaging/signing-key.asc "$site/key.asc"

# ---- apt: a flat repository ("deb URL ./"); every architecture in one index.
deb=$site/deb
rm -rf "$deb"
mkdir -p "$deb"
cp "$in"/*.deb "$deb/"
(
  cd "$deb"
  apt-ftparchive packages . >Packages
  gzip -9kn Packages
  apt-ftparchive \
    -o APT::FTPArchive::Release::Origin="$NAME" \
    -o APT::FTPArchive::Release::Label="$NAME" \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Description="$NAME packages" \
    release . >../Release.tmp
  mv ../Release.tmp Release
  sign --clearsign -o InRelease Release
  sign --detach-sign --armor -o Release.gpg Release
)

# ---- dnf: signed packages and signed metadata.
rpm=$site/rpm
rm -rf "$rpm"
mkdir -p "$rpm"
cp "$in"/*.rpm "$rpm/"
# Name gpg explicitly: some distributions point rpm at a gpg2 that may not exist.
rpmsign --define "_gpg_name $KEY" --define "__gpg $(command -v gpg)" --addsign "$rpm"/*.rpm >/dev/null
createrepo_c --quiet "$rpm"
sign --detach-sign --armor -o "$rpm/repodata/repomd.xml.asc" "$rpm/repodata/repomd.xml"
cat >"$rpm/$NAME.repo" <<REPO
[$NAME]
name=$NAME
baseurl=$SITE_URL/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$SITE_URL/key.asc
REPO

# ---- index page
version=$(ls "$in"/*.deb | sed -n "s|.*/${NAME}_\([^_]*\)_.*|\1|p" | head -1)
fpr_spaced=$(sed 's/.\{4\}/& /g; s/ $//' <<<"$KEY")
sed -e "s|@NAME@|$NAME|g" -e "s|@URL@|$SITE_URL|g" -e "s|@FPR@|$KEY|g" \
  -e "s|@FPR_SPACED@|$fpr_spaced|g" -e "s|@VERSION@|$version|g" -e "s|@GUI@|$GUI|g" \
  packaging/site-index.html >"$site/index.html"
echo "$site"
