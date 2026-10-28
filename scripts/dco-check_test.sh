#!/usr/bin/env bash
#
# Tests for dco-check.sh.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
g() { git -C "$work" -c user.name=Dev -c user.email=dev@example.com -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }

g init -q
g commit -q --allow-empty -m base
base=$(g rev-parse HEAD)

g commit -q --allow-empty -m "feat: signed" -m "Signed-off-by: Dev <dev@example.com>"
(cd "$work" && bash "$here/dco-check.sh" "$base" HEAD >/dev/null) || fail "signed commit rejected"

g commit -q --allow-empty -m "feat: other signer" -m "Signed-off-by: Someone <else@example.com>"
if (cd "$work" && bash "$here/dco-check.sh" "$base" HEAD >/dev/null); then fail "wrong signer accepted"; fi

g reset -q --hard HEAD~1
g commit -q --allow-empty -m "feat: unsigned"
out=$(cd "$work" && bash "$here/dco-check.sh" "$base" HEAD) && fail "unsigned commit accepted"
echo "$out" | grep -q "missing Signed-off-by for dev@example.com" || fail "no offender listed: $out"

# Dependabot cannot sign off; its commits are exempt.
base2=$(g rev-parse HEAD)
git -C "$work" -c user.name="dependabot[bot]" -c user.email="49699333+dependabot[bot]@users.noreply.github.com" -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -q --allow-empty -m "build: bump x"
(cd "$work" && bash "$here/dco-check.sh" "$base2" HEAD >/dev/null) || fail "dependabot commit rejected"

# Email case doesn't matter.
git -C "$work" -c user.name=Dev -c user.email=Dev@Example.com -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -q --allow-empty -m "feat: mixed case" -m "Signed-off-by: Dev <dev@example.com>"
(cd "$work" && bash "$here/dco-check.sh" "$base2" HEAD >/dev/null) || fail "case-only email difference rejected"

echo "PASS: dco-check_test.sh"
