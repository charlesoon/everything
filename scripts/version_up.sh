#!/usr/bin/env bash
# version_up.sh — Bump the app version across every file that pins it.
#
# Usage:
#   scripts/version_up.sh 0.5.2
#
# Updates (structural, anchored replacements — no blind global search/replace):
#   - package.json                (top-level "version")
#   - package-lock.json           ("everything" package: root + packages."")
#   - app/Cargo.toml        ([package] version)
#   - app/Cargo.lock        ([[package]] name = "everything")
set -euo pipefail

# ---------------------------------------------------------------------------
# Args & validation
# ---------------------------------------------------------------------------
NEW_VERSION="${1:-}"

if [[ -z "$NEW_VERSION" ]]; then
  echo "Usage: scripts/version_up.sh <new-version>   (e.g. scripts/version_up.sh 0.5.2)" >&2
  exit 1
fi

# Accept X.Y.Z with optional -prerelease / +build metadata.
if [[ ! "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "Error: '$NEW_VERSION' is not a valid semver (expected X.Y.Z)." >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

PKG_JSON="package.json"
PKG_LOCK="package-lock.json"
CARGO_TOML="app/Cargo.toml"
CARGO_LOCK="app/Cargo.lock"

# Regex-escaped version for the verification step (dots are literal).
NEW_RE="${NEW_VERSION//./\\.}"

CURRENT_VERSION="$(perl -0777 -ne 'print $1 if /name\s*=\s*"everything"\s*version\s*=\s*"([^"]*)"/' "$CARGO_TOML")"
echo "Bumping version: ${CURRENT_VERSION:-?} -> $NEW_VERSION"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
# Fail if the expected anchor isn't present in a file (catches renames/drift).
require_file() {
  [[ -f "$1" ]] || { echo "Error: missing file '$1'." >&2; exit 1; }
}

# Verify the new version landed at the expected structural spot.
verify() { # <file> <perl-regex>
  perl -0777 -ne 'exit(($_ =~ /'"$2"'/) ? 0 : 1)' "$1" \
    || { echo "Error: verification failed for '$1' (anchor not updated)." >&2; exit 1; }
}

for f in "$PKG_JSON" "$PKG_LOCK" "$CARGO_TOML" "$CARGO_LOCK"; do
  require_file "$f"
done

# ---------------------------------------------------------------------------
# Edits
# ---------------------------------------------------------------------------
# package.json — first (top-level) "version" key.
perl -0777 -pi -e 's/("version"\s*:\s*")[^"]*"/${1}'"$NEW_VERSION"'"/' "$PKG_JSON"

# package-lock.json — both "version" fields that belong to the "everything"
# package (root object + packages.""). Anchored on the preceding name so
# dependency versions are never touched.
perl -0777 -pi -e 's/("name"\s*:\s*"everything",\s*"version"\s*:\s*")[^"]*"/${1}'"$NEW_VERSION"'"/g' "$PKG_LOCK"

# Cargo.toml — [package] version (the one right after name = "everything").
perl -0777 -pi -e 's/(name\s*=\s*"everything"\s*version\s*=\s*")[^"]*"/${1}'"$NEW_VERSION"'"/' "$CARGO_TOML"

# Cargo.lock — the [[package]] entry for "everything".
perl -0777 -pi -e 's/(name = "everything"\s*version = ")[^"]*"/${1}'"$NEW_VERSION"'"/' "$CARGO_LOCK"

# ---------------------------------------------------------------------------
# Verify every anchor was actually updated
# ---------------------------------------------------------------------------
verify "$PKG_JSON"    '"version"\s*:\s*"'"$NEW_RE"'"'
verify "$PKG_LOCK"    '"name"\s*:\s*"everything",\s*"version"\s*:\s*"'"$NEW_RE"'"'
verify "$CARGO_TOML"  'name\s*=\s*"everything"\s*version\s*=\s*"'"$NEW_RE"'"'
verify "$CARGO_LOCK"  'name = "everything"\s*version = "'"$NEW_RE"'"'

echo "Done. Updated 4 files to $NEW_VERSION:"
echo "  - $PKG_JSON"
echo "  - $PKG_LOCK"
echo "  - $CARGO_TOML"
echo "  - $CARGO_LOCK"
