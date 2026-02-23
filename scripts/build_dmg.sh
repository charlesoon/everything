#!/usr/bin/env bash
# build_dmg.sh — Sign, build, notarize, and staple a release DMG for Everything (macOS)
set -euo pipefail

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CERTS_DIR="$REPO_ROOT/certificates"
IMPORT_JSON="$CERTS_DIR/import.json"

# ---------------------------------------------------------------------------
# Read credentials from import.json
# ---------------------------------------------------------------------------
export APPLE_ID="$(python3 -c "import json; d=json.load(open('$IMPORT_JSON')); print(d['apple']['apple_id'])")"
export APPLE_PASSWORD="$(python3 -c "import json; d=json.load(open('$IMPORT_JSON')); print(d['apple']['apple_password'])")"
export APPLE_TEAM_ID="$(python3 -c "import json; d=json.load(open('$IMPORT_JSON')); print(d['apple']['team_id'])")"
SIGNING_IDENTITY="$(python3 -c "import json; d=json.load(open('$IMPORT_JSON')); print(d['apple']['developer_id'])")"

# ---------------------------------------------------------------------------
# Read app info from tauri.conf.json
# ---------------------------------------------------------------------------
TAURI_CONF="$REPO_ROOT/src-tauri/tauri.conf.json"
APP_VERSION="$(python3 -c "import json; d=json.load(open('$TAURI_CONF')); print(d.get('version', d.get('package',{}).get('version','')))")"
APP_NAME="Everything"
DMG_NAME="${APP_NAME}_${APP_VERSION}_aarch64.dmg"

BUNDLE_DIR="$REPO_ROOT/src-tauri/target/aarch64-apple-darwin/release/bundle"
APP_PATH="$BUNDLE_DIR/macos/${APP_NAME}.app"
DMG_PATH="$BUNDLE_DIR/dmg/${DMG_NAME}"
RELEASE_DIR="$REPO_ROOT/releases"

# ---------------------------------------------------------------------------
# Restore tauri.conf.json signingIdentity to null on exit
# ---------------------------------------------------------------------------
restore_conf() {
  python3 - <<PYEOF
import json
with open('$TAURI_CONF') as f:
    conf = json.load(f)
conf['bundle']['macOS']['signingIdentity'] = None
with open('$TAURI_CONF', 'w') as f:
    json.dump(conf, f, indent=2)
    f.write('\n')
PYEOF
  echo "    tauri.conf.json signingIdentity restored to null"
}
trap restore_conf EXIT

# ---------------------------------------------------------------------------
# Step 1: Verify signing identity is already installed in keychain
# ---------------------------------------------------------------------------
echo "==> [1/6] Verifying signing identity in keychain"
if ! security find-identity -v -p codesigning | grep -qF "$SIGNING_IDENTITY"; then
  echo "ERROR: Signing identity not found in keychain: $SIGNING_IDENTITY"
  echo "       Install the Developer ID certificate via Keychain Access and retry."
  exit 1
fi
echo "    Found: $SIGNING_IDENTITY"

# ---------------------------------------------------------------------------
# Step 2: Set signing identity in tauri.conf.json (restored on exit)
# ---------------------------------------------------------------------------
echo "==> [2/6] Configuring Tauri signing identity"
python3 - <<EOF
import json
with open('$TAURI_CONF') as f:
    conf = json.load(f)
conf.setdefault('bundle', {}).setdefault('macOS', {})['signingIdentity'] = '$SIGNING_IDENTITY'
with open('$TAURI_CONF', 'w') as f:
    json.dump(conf, f, indent=2)
    f.write('\n')
EOF
echo "    signingIdentity set"

# ---------------------------------------------------------------------------
# Step 3: Build release (Tauri signs the .app and creates the DMG)
# ---------------------------------------------------------------------------
echo "==> [3/6] Building release with Tauri"
cd "$REPO_ROOT"
npm run tauri build -- --target aarch64-apple-darwin

# ---------------------------------------------------------------------------
# Step 4: Verify code signature
# ---------------------------------------------------------------------------
echo "==> [4/6] Verifying code signature"
codesign --verify --deep --strict --verbose=2 "$APP_PATH"
spctl --assess --type execute --verbose "$APP_PATH"
echo "    Signature OK"

# ---------------------------------------------------------------------------
# Step 5: Notarize the DMG and staple the ticket
# ---------------------------------------------------------------------------
echo "==> [5/6] Notarizing: $DMG_PATH"

xcrun notarytool submit "$DMG_PATH" \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" \
  --wait \
  --output-format plist \
  | tee /tmp/notarize_result.plist

NOTARIZE_STATUS="$(python3 -c "
import plistlib
with open('/tmp/notarize_result.plist', 'rb') as f:
    d = plistlib.load(f)
print(d.get('status', 'unknown'))
")"

if [ "$NOTARIZE_STATUS" != "Accepted" ]; then
  echo "ERROR: Notarization failed with status: $NOTARIZE_STATUS"
  exit 1
fi
echo "    Notarization accepted"

xcrun stapler staple "$DMG_PATH"
xcrun stapler validate "$DMG_PATH"
echo "    Ticket stapled"

# ---------------------------------------------------------------------------
# Step 6: Copy to releases/
# ---------------------------------------------------------------------------
echo "==> [6/6] Copying to releases/"
mkdir -p "$RELEASE_DIR"
cp "$DMG_PATH" "$RELEASE_DIR/$DMG_NAME"
echo "    Copied: $RELEASE_DIR/$DMG_NAME"

echo ""
echo "======================================================"
echo "  Release DMG ready: $RELEASE_DIR/$DMG_NAME"
echo "======================================================"
