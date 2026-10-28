#!/usr/bin/env bash
#
# Fail unless every non-merge commit in <base>..<head> carries a
# Signed-off-by trailer for its author's email address.
#
#   scripts/dco-check.sh <base> <head>
set -euo pipefail

bad=0
for sha in $(git rev-list --no-merges "$1..$2"); do
  email=$(git log -1 --format=%ae "$sha")
  # Dependabot can't sign off. Its changes are reviewed in the PR instead.
  [ "$email" = "49699333+dependabot[bot]@users.noreply.github.com" ] && continue
  if ! git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$sha" \
      | grep -Fiq "<$email>"; then
    echo "missing Signed-off-by for $email: $(git log -1 --format='%h %s' "$sha")"
    bad=1
  fi
done

if [ "$bad" -ne 0 ]; then
  echo
  echo "Sign off each commit with 'git commit -s'. See CONTRIBUTING.md."
  exit 1
fi
echo "All commits are signed off."
