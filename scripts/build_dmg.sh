#!/usr/bin/env bash
# Native GPUI release packaging; signing/notarization use certificates/import.json.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$REPO_ROOT/scripts/bundle_macos.py" --dmg "$@"
