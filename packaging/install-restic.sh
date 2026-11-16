#!/bin/sh
# Installs the pinned restic release, the snapshot engine, after checking its SHA-256.
# Usage: install-restic.sh [target-dir] (default /usr/local/lib/upkeep/bin).
# Set RESTIC_ARCHIVE to a downloaded restic_<version>_linux_<arch>.bz2 to install without network.
set -eu

version=0.19.1
case "$(uname -m)" in
  x86_64) arch=amd64 sha256=f415415624dcc452f2a02b8c33641791a8c6d6d3b65bbb3543fcf9a25151585c ;;
  aarch64) arch=arm64 sha256=a5f64aaab53d51e311fa3829124c5b703f2d14cf187d8640b6be3b2b49376465 ;;
  *) echo "no pinned restic for $(uname -m)" >&2; exit 1 ;;
esac
target=${1:-/usr/local/lib/upkeep/bin}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
archive="$work/restic.bz2"
if [ -n "${RESTIC_ARCHIVE:-}" ]; then
  cp "$RESTIC_ARCHIVE" "$archive"
else
  curl -fsSL -o "$archive" "https://github.com/restic/restic/releases/download/v$version/restic_${version}_linux_$arch.bz2"
fi
echo "$sha256  $archive" | sha256sum -c --quiet -
# Minimal images lack bzip2; python3 is always there on Ubuntu.
if command -v bunzip2 >/dev/null; then
  bunzip2 "$archive"
else
  python3 -c 'import bz2, shutil, sys; shutil.copyfileobj(bz2.open(sys.argv[1]), open(sys.argv[2], "wb"))' "$archive" "$work/restic"
fi
install -D -m 755 "$work/restic" "$target/restic"
"$target/restic" version
