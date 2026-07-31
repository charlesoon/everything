# macOS Daemon-Writer Verification Checklist

Source design: `design-macos-daemon-writer.html` (v3) §9. Item numbers below
match that document. Automated items run via `scripts/verify_macos_daemon.sh`
(throwaway home/data dir; your `com.everything.app` defaults are saved and
restored). Items marked **[device]** need a signed, notarized bundle built by
`scripts/build_dmg.sh`, installed in `/Applications` — SMAppService behaves
differently for unsigned/translocated binaries.

Reset for a fresh-install run: `defaults delete com.everything.app` and delete
`~/Library/Application Support/com.everything.app/` (then re-grant FDA if you
also reset TCC via `tccutil reset SystemPolicyAllFiles com.everything.app`).

| # | Check | How | Status |
|---|-------|-----|--------|
| 1 | Fresh install: gate → FDA onboarding → decision → registration starts daemon immediately (G3) → fresh index | **[device]** first launch after reset: modal shows; daemon stays `WaitingOnboarding` (`nc -U` status) until 시작하기/나중에; `ps aux \| grep -- --daemon` right after deciding | auto: gate+release ✔ (script) / modal+register: device |
| 2 | Re-login, no GUI: daemon catches up; MCP search sees new files | **[device]** log out/in, create a file, `everything --mcp` search for it | device |
| 3 | GUI read-only: zero write errors, search works | run app, use it; `logs/everything.log` has no GUI write errors; searches fine | device |
| 4 | rename/trash from the GUI reflected in 1–2s | rename + trash a file in the app; re-search | device |
| 5 | `reset_index` + 600s cooldown | Reset button; second reset within 10 min shows cooldown toast | ✔ script (IPC level) |
| 6 | App toggle OFF → banner + lease keeps updating while GUI open; daemon exits ≤5min after last user | 자동 실행 토글 OFF; watch `ps`; close app; wait ≤5min | device |
| 7 | System Settings Login Items OFF → no full daemon respawn; lease only (G1) | toggle off in System Settings; use GUI/MCP; `ps` shows `--daemon --lease` only | device |
| 8 | Toggle back ON → lease cleared → immediate start (G5) | 자동 실행 ON; daemon (non-lease) up within seconds | device |
| 9 | Update → exe-identity restart; re-register only when the embedded plist changed (marker) + note BTM notification behavior (G7) | install new build over old while daemon runs; daemon exits(1) ≤5s, launchd relaunches new binary; `agent_plist.marker` unchanged unless plist changed | device |
| 10 | FDA skipped → degraded surfaced in banner and MCP responses | skip onboarding; status shows `fdaOk:false`; MCP search output carries the degraded note | device (MCP note: ✔ code path) |
| 11 | `kill -9` daemon → launchd relaunches | **[device]** `kill -9 <pid>`; new pid within ~10s (`launchctl print gui/$UID/com.everything.app.daemon`) | device |
| 12 | Manual duplicate `--daemon` → exit 0, no loop | second `--daemon` while one runs | ✔ script |
| 13 | Second GUI instance runs read-only | open the app twice | device |
| 14 | Dev mode: registration skipped + spawn fallback | `npm run tauri dev`: bridge log shows spawn, `--register-daemon` fails cleanly | ✔ (observed in dev runs) |
| 15 | MCP-only: WaitingOnboarding guidance + 7-day escape hatch (Q7) | fresh install, only `--mcp`: search answers with "launch the app once"; simulate escape: `defaults write com.everything.app onboardingFirstSeen -int <epoch-8d>` + touch `mcp.activity`, wait one gate tick | ✔ message path / escape: device or simulated |
| 16 | App moved → self-heal re-register / translocation notice | move .app; relaunch | device |
| 17 | Search spam during VACUUM → S2 retry, no user-visible error | trigger reset then hold a key in the search box during rebuild/finalize | device |
| 18 | Toggle OFF then relaunch app → no re-register/re-spawn (gate) | OFF → quit → relaunch → `ps`: lease only, no full daemon, registration stays off | device |
| 19 | App deleted → daemon self-exits ≤15s (3 ticks); ghost Login Items entry noted; `--uninstall` cleans up | delete .app while daemon runs; watch `ps`; check System Settings; run `everything --uninstall` from a backup copy | device |
| 20 | External dir rename (Finder/terminal `mv`) → whole subtree searchable ≤10s; old subtree gone (S5/V2) | | ✔ script |
| 21 | `git clone`/`npm install` storms → no rescan-queue blowup (G11) | clone a big repo; `logs/everything.log`: no probe-cap spam, single-flight rescans only | device (cap+coalesce: ✔ unit) |
| 22 | FDA granted later → rising edge (≤60s) → forced catchup pulls protected folders in | skip onboarding → grant FDA in System Settings → wait ≤60s → protected paths searchable, banner clears | device |
| 23 | disable/restart mid-fresh-build → unwind ≤2s, quiesce bounded | | ✔ script (disable path) |
| 24 | Logout/shutdown SIGTERM (mid-build too) → clean catchup on next login (G4) | | ✔ script (SIGTERM path) / full logout: device |
| 25 | Rollback: replace bundle with the previous release → new daemon exits ≤5s → old daemon runs; no dual-writer ALERT in diag log | | device |
| 26 | Socket path >104 bytes → `$TMPDIR` fallback + sidecar + path in status (G13) | | ✔ script env (observed in every scratchpad run) + unit test |

P1 real-device items recorded separately (design §9 P1): ① objc2-service-management
register/unregister/status on a signed bundle, ② `register()` starts the daemon
immediately, ③ the failed FDA probe auto-lists the app in the FDA pane,
④ when the BTM "background item added" notification fires (first register vs
re-register). Note findings here after the first signed-build session.
