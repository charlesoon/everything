#!/usr/bin/env bash
# Build/sign/notarize a DMG, copy it to releases/, and update latest-mac.yml.
# Signing/notarization use certificates/import.json.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$REPO_ROOT/scripts/bundle_macos.py" --dmg "$@"
