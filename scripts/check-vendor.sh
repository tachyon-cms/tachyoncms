#!/usr/bin/env bash
#
# Fail unless vendor/avif-decode is the published avif-decode crate with
# vendor/avif-decode.patch applied, and nothing else.
#
#   scripts/check-vendor.sh           check
#   scripts/check-vendor.sh --update  rewrite the patch from vendor/avif-decode
set -euo pipefail

version=3.0.0
# The cksum crates.io's index records for avif-decode 3.0.0, which cargo checks too.
sha256=1eee11893966d77d3e18c66fdcfb1c30e2d8df050346609c775ce8eec3390344

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -sSfL -o "$work/crate.tar.gz" "https://static.crates.io/crates/avif-decode/avif-decode-$version.crate"
echo "$sha256  $work/crate.tar.gz" | sha256sum -c --quiet -
tar -xzf "$work/crate.tar.gz" -C "$work"
published=$work/avif-decode-$version
g() { git -C "$published" -c user.name=x -c user.email=x@x -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }

if [ "${1:-}" = --update ]; then
  g init -q && g add -A && g commit -q -m published
  find "$published" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
  cp -R "$root/vendor/avif-decode/." "$published/"
  g add -A
  g diff --cached --binary > "$root/vendor/avif-decode.patch"
  echo "Wrote vendor/avif-decode.patch."
  exit 0
fi

git -C "$published" apply --binary "$root/vendor/avif-decode.patch"
if ! diff -r "$published" "$root/vendor/avif-decode"; then
  echo
  echo "vendor/avif-decode differs from avif-decode $version plus vendor/avif-decode.patch."
  echo "See vendor/README.md."
  exit 1
fi
echo "vendor/avif-decode is avif-decode $version plus vendor/avif-decode.patch."
