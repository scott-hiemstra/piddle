# Shared by build-site.sh and build-site-arch.sh. Source it from the repo root.
set -euo pipefail

NAME=piddle
GUI=piddle-gui
SITE_URL=${SITE_URL:-https://scott-hiemstra.github.io/$NAME}

# The signing key: the GnuPG home's secret key, which must be the published one.
KEY=$(gpg --batch --list-secret-keys --with-colons 2>/dev/null | awk -F: '/^fpr/{print $10; exit}')
PUBLISHED=$(gpg --batch --show-keys --with-colons packaging/signing-key.asc | awk -F: '/^fpr/{print $10; exit}')
if [ -z "$KEY" ]; then
  echo "no secret key in the GnuPG home (${GNUPGHOME:-~/.gnupg}); import the signing key first" >&2
  exit 1
fi
if [ "$KEY" != "$PUBLISHED" ]; then
  echo "the secret key $KEY is not the published key $PUBLISHED (packaging/signing-key.asc)" >&2
  exit 1
fi

sign() {
  gpg --batch --yes --quiet --pinentry-mode loopback --passphrase '' --local-user "$KEY" "$@"
}
