#!/usr/bin/env bash
# Native GPUI release packaging. Signing/notarization are explicit environment options.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$REPO_ROOT/scripts/bundle_macos.py" --dmg "$@"
