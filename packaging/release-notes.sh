#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version, for use as release notes.
# Fails if the section is missing, empty, or still marked Unreleased, so a
# release can't go out without notes.
#   packaging/release-notes.sh 0.4.0 > notes.md
set -euo pipefail
version=${1:?usage: release-notes.sh VERSION}
changelog="$(dirname "$0")/../CHANGELOG.md"

heading=$(grep -m1 -F "## [$version]" "$changelog" || true)
if [ -z "$heading" ]; then
  echo "error: CHANGELOG.md has no section for $version" >&2
  exit 1
fi
if [[ "$heading" == *Unreleased* ]]; then
  echo "error: CHANGELOG.md section for $version is still marked Unreleased; set the release date" >&2
  exit 1
fi
# Body: lines after the heading, up to the next version heading or link list.
notes=$(awk -v h="$heading" '
  $0 == h { on = 1; next }
  on && (/^## \[/ || /^\[[^]]+\]: /) { exit }
  on { print }
' "$changelog" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')
if [ -z "${notes//[[:space:]]/}" ]; then
  echo "error: CHANGELOG.md section for $version is empty" >&2
  exit 1
fi
printf '%s\n' "$notes"
