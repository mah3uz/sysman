#!/usr/bin/env bash
# Prints one release's section of CHANGELOG.md, with a compare link to the release before it:
#   release-notes.sh <version|Unreleased>
set -euo pipefail

v=${1:?usage: release-notes.sh <version|Unreleased>}
changelog=$(cd "$(dirname "$0")/.." && pwd)/CHANGELOG.md

notes=$(awk -v v="$v" '
  /^## / { if (found) exit; found = ($2 == v) ; next }
  found { print }
' "$changelog" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')

if [[ $v != Unreleased ]]; then
  [[ -n $notes ]] || { echo "release-notes: no \"## $v\" section in CHANGELOG.md" >&2; exit 1; }
  prev=$(awk -v v="$v" '/^## / && $2 != "Unreleased" { if (seen) { print $2; exit } if ($2 == v) seen = 1 }' "$changelog")
  if [[ -n $prev ]]; then
    notes+=$'\n\n'"**Full changelog**: https://github.com/mah3uz/sysman/compare/v$prev...v$v"
  fi
fi
printf '%s\n' "$notes"
