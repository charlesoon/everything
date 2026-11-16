#!/bin/bash
# Automated subset of docs/macos-daemon-writer-verification.md.
#
# Runs the daemon against a THROWAWAY home/data dir (never your real index)
# and asserts the §4 lifecycle contract: onboarding gate, UDS status/perms,
# build, S5 rename rescan, reset cooldown, disable/SIGTERM exit codes,
# duplicate dormancy. Your com.everything.app defaults (onboarding/opt-out)
# are saved and restored around the run.
#
# Usage:  scripts/verify_macos_daemon.sh [path-to-everything-binary]
# Default binary: app/target/debug/everything (build with `cargo build`).
set -u

BIN="${1:-app/target/debug/everything}"
[ -x "$BIN" ] || { echo "binary not found: $BIN (cargo build first)"; exit 2; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok  - $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL- $1"; }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }

DOMAIN=com.everything.app
SAVED_DECISION=$(defaults read $DOMAIN onboardingDecision 2>/dev/null || echo "__none__")
SAVED_OPTOUT=$(defaults read $DOMAIN daemonOptOut 2>/dev/null || echo "__none__")
SAVED_FIRSTSEEN=$(defaults read $DOMAIN onboardingFirstSeen 2>/dev/null || echo "__none__")
restore_prefs() {
  for pair in "onboardingDecision:$SAVED_DECISION" "daemonOptOut:$SAVED_OPTOUT" "onboardingFirstSeen:$SAVED_FIRSTSEEN"; do
    key="${pair%%:*}"; val="${pair#*:}"
    if [ "$val" = "__none__" ]; then defaults delete $DOMAIN "$key" 2>/dev/null || true
    else defaults write $DOMAIN "$key" "$val"; fi
  done
}

TMP=$(mktemp -d /tmp/everything-verify.XXXXXX)
HOME_DIR="$TMP/home"; DATA_DIR="$TMP/data"
mkdir -p "$HOME_DIR/docs" "$DATA_DIR"
echo hello > "$HOME_DIR/docs/hello.txt"
echo world > "$HOME_DIR/docs/world.txt"
echo top   > "$HOME_DIR/readme.md"
touch "$DATA_DIR/index.db"   # EVERYTHING_MCP_DB honors existing files only

DPID=""
cleanup() {
  [ -n "$DPID" ] && kill -9 "$DPID" 2>/dev/null
  restore_prefs
  rm -rf "$TMP"
}
trap cleanup EXIT

sock() { cat "$DATA_DIR/daemon.sock.path" 2>/dev/null || echo "$DATA_DIR/daemon.sock"; }
send() { printf '%s\n' "$1" | nc -U "$(sock)" 2>/dev/null; }
start_daemon() {
  HOME="$HOME_DIR" EVERYTHING_MCP_DB="$DATA_DIR/index.db" "$BIN" --daemon >> "$TMP/daemon.log" 2>&1 &
  DPID=$!
}
wait_state() { # $1=state substring  $2=timeout secs
  local deadline=$((SECONDS + $2))
  while [ $SECONDS -lt $deadline ]; do
    send '{"cmd":"status"}' | grep -q "\"state\":\"$1\"" && return 0
    sleep 1
  done
  return 1
}

echo "== everything macOS daemon verification (bin: $BIN, tmp: $TMP) =="
defaults delete $DOMAIN onboardingDecision 2>/dev/null || true
defaults delete $DOMAIN daemonOptOut 2>/dev/null || true
defaults delete $DOMAIN onboardingFirstSeen 2>/dev/null || true

echo "-- onboarding gate + IPC"
start_daemon
sleep 2
STATUS=$(send '{"cmd":"status"}')
echo "$STATUS" | grep -q '"state":"WaitingOnboarding"'; check $? "first install holds in WaitingOnboarding (#1)"
[ -z "$(sqlite3 "$DATA_DIR/index.db" 'SELECT name FROM sqlite_master' 2>/dev/null)" ]
check $? "no schema/build happens while gated (#1)"
PERMS=$(stat -f '%Lp' "$(sock)" 2>/dev/null)
[ "$PERMS" = "600" ]; check $? "socket is chmod 0600 (D5) [got: ${PERMS:-missing}]"

echo "-- duplicate daemon dormancy (#12)"
HOME="$HOME_DIR" EVERYTHING_MCP_DB="$DATA_DIR/index.db" "$BIN" --daemon > "$TMP/dup.log" 2>&1
check $? "second --daemon exits 0 (dormant), no loop"

echo "-- gate release -> build -> Ready"
defaults write $DOMAIN onboardingDecision skipped
wait_state Ready 30; check $? "decision releases the gate; daemon reaches Ready"
COUNT=$(sqlite3 "$DATA_DIR/index.db" "SELECT COUNT(*) FROM entries" 2>/dev/null)
[ "${COUNT:-0}" -ge 4 ]; check $? "fresh build indexed the home tree (entries=$COUNT)"

echo "-- S5: external directory rename reflects the subtree (#20)"
mv "$HOME_DIR/docs" "$HOME_DIR/papers"
deadline=$((SECONDS + 20)); okflag=1
while [ $SECONDS -lt $deadline ]; do
  HIT=$(sqlite3 "$DATA_DIR/index.db" "SELECT COUNT(*) FROM entries WHERE path LIKE '%/papers/%'" 2>/dev/null)
  if [ "${HIT:-0}" -ge 2 ]; then okflag=0; break; fi
  sleep 1
done
check $okflag "renamed dir's children re-indexed under the new path (hit=$HIT)"
# The rescan (new subtree) and the watcher batch (old-subtree range delete)
# land independently — poll rather than snapshot.
deadline=$((SECONDS + 15)); okflag=1
while [ $SECONDS -lt $deadline ]; do
  OLD=$(sqlite3 "$DATA_DIR/index.db" "SELECT COUNT(*) FROM entries WHERE path LIKE '%/docs%'" 2>/dev/null)
  if [ "${OLD:-1}" = "0" ]; then okflag=0; break; fi
  sleep 1
done
check $okflag "old subtree fully removed (#20/V2) [remnants=$OLD]"

echo "-- reset + cooldown (#5)"
R1=$(send '{"cmd":"reset_index"}')
echo "$R1" | grep -q '"ok":true'; check $? "reset_index accepted"
R2=$(send '{"cmd":"reset_index"}')
echo "$R2" | grep -q '"error":"cooldown"'; check $? "second reset within 600s refused with cooldown"
wait_state Ready 30; check $? "rebuild after reset reaches Ready"

echo "-- disable -> quiesce -> exit(0) (#23)"
send '{"cmd":"disable"}' > /dev/null
wait "$DPID"; EXIT=$?
[ "$EXIT" = "0" ]; check $? "disable exits 0 (dormant) [got: $EXIT]"
DPID=""

echo "-- SIGTERM mid-run -> exit(0) (#24)"
start_daemon
sleep 2
kill -TERM "$DPID"
wait "$DPID"; EXIT=$?
[ "$EXIT" = "0" ]; check $? "SIGTERM quiesces to exit 0 [got: $EXIT]"
DPID=""

echo "-- registration state (informational)"
if launchctl print "gui/$(id -u)/com.everything.app.daemon" > /dev/null 2>&1; then
  echo "  info- login-item agent IS registered for this user (launchctl print ok)"
else
  echo "  info- login-item agent not registered (expected for dev builds; use a signed .app + --register-daemon)"
fi

echo
echo "== result: $PASS passed, $FAIL failed (log: $TMP/daemon.log kept only on failure) =="
if [ "$FAIL" -gt 0 ]; then
  cp "$TMP/daemon.log" "./verify_macos_daemon.failure.log" 2>/dev/null
  echo "daemon log copied to ./verify_macos_daemon.failure.log"
  exit 1
fi
exit 0
