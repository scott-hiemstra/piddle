#!/usr/bin/env bash
# Add pacman repositories (arch/x86_64, arch/aarch64) to the package
# repository site, from one release's .pkg.tar.zst files. Runs on Arch Linux
# (it needs repo-add). See build-site.sh for the rest of the site.
#
#   packaging/build-site-arch.sh <dir with the release's packages> <site dir>
set -euo pipefail
cd "$(dirname "$0")/.."
. packaging/site-common.sh

in=$(realpath "$1")
site=$(realpath -m "$2")
for arch in x86_64 aarch64; do
  dir=$site/arch/$arch
  rm -rf "$dir"
  mkdir -p "$dir"
  pkgs=("$in"/*-"$arch".pkg.tar.zst)
  [ -e "${pkgs[0]}" ] || { echo "no $arch packages in $in" >&2; exit 1; }
  cp "${pkgs[@]}" "$dir/"
  for p in "$dir"/*.pkg.tar.zst; do
    sign --detach-sign -o "$p.sig" "$p"
  done
  repo-add --quiet --sign --key "$KEY" "$dir/$NAME.db.tar.zst" "$dir"/*.pkg.tar.zst
  # repo-add links $NAME.db to the archive; Pages serves files, so copy.
  for f in "$NAME.db" "$NAME.db.sig" "$NAME.files" "$NAME.files.sig"; do
    [ -L "$dir/$f" ] && cp --remove-destination "$(readlink -f "$dir/$f")" "$dir/$f"
  done
  rm -f "$dir"/*.old "$dir"/*.old.sig
done
echo "$site/arch"
