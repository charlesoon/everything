; NSIS installer hooks for Everything — Windows admin indexer service.
; Inserted by windows/installer.nsi.
; Design: design-windows-service-writer.html §3.2 (perMachine install, D1/D5).
;
; Principles:
;   * The installer runs elevated (perMachine). Fatal steps Abort on failure;
;     hardening steps (recovery policy) only warn.
;   * Destructive cleanup of a previous install runs ONLY after every fatal step
;     succeeds, so a mid-way failure leaves the old install intact — recoverable
;     via the in-app `--register-service` banner (D5).
;   * Preinstall bounds graceful stop, then verifies and terminates the actual
;     indexer PID if needed, preserving DB/WAL/SHM and preventing restarts.
;     Startup settings are restored on install failure or cancellation.
;   * sc start is async, so every state change is confirmed by polling.
;     Only exit code 1060 (ERROR_SERVICE_DOES_NOT_EXIST) counts as "not
;     installed"; any other sc failure Aborts rather than being mistaken for
;     stopped (so a partial/corrupt SCM state is never papered over).
;   * LogicLib (${If}/${EndIf}) is provided by installer.nsi.

!define EVERYTHING_SERVICE "EverythingIndexer"
; With `SetShellVarContext all`, $APPDATA resolves to <ProgramData>. Expanded at
; run time, so every hook sets the context before using this.
!define EVERYTHING_DATA_ROOT "$APPDATA\com.everything.app"

; ---------------------------------------------------------------------------
; Polling helpers. Internal labels are UID-suffixed so a macro can be inserted
; more than once (e.g. WAIT_SERVICE_STOPPED in both PREINSTALL and PREUNINSTALL)
; without duplicate-label errors.
; ---------------------------------------------------------------------------

!macro WAIT_SERVICE_STOPPED UID
  ; 500ms x 60 = 30s. 1060 => not installed (treat as stopped, e.g. first
  ; install). Any other non-zero sc exit is an SCM fault => Abort.
  StrCpy $R1 0
  stopped_loop_${UID}:
    nsExec::Exec 'cmd /c sc query ${EVERYTHING_SERVICE}'
    Pop $0
    ${If} $0 == 1060
      Goto stopped_done_${UID}
    ${ElseIf} $0 != 0
      Abort "SCM query failed (sc query exit=$0)"
    ${EndIf}
    nsExec::Exec 'cmd /c sc query ${EVERYTHING_SERVICE} | findstr /C:"STOPPED"'
    Pop $0
    ${If} $0 == 0
      Goto stopped_done_${UID}
    ${EndIf}
    IntOp $R1 $R1 + 1
    ${If} $R1 > 60
      Abort "${EVERYTHING_SERVICE} did not stop within 30s"
    ${EndIf}
    Sleep 500
    Goto stopped_loop_${UID}
  stopped_done_${UID}:
!macroend

!macro WAIT_SERVICE_RUNNING UID
  ; sc start success != running. Confirm RUNNING (D2: reached quickly since
  ; indexing happens after RUNNING). 500ms x 60 = 30s.
  StrCpy $R1 0
  running_loop_${UID}:
    nsExec::Exec 'cmd /c sc query ${EVERYTHING_SERVICE} | findstr /C:"RUNNING"'
    Pop $0
    ${If} $0 == 0
      Goto running_done_${UID}
    ${EndIf}
    IntOp $R1 $R1 + 1
    ${If} $R1 > 60
      Abort "${EVERYTHING_SERVICE} did not reach RUNNING within 30s"
    ${EndIf}
    Sleep 500
    Goto running_loop_${UID}
  running_done_${UID}:
!macroend

!macro WAIT_SERVICE_DELETED UID
  ; Poll until sc query returns 1060. Non-fatal: a marked-for-delete service
  ; clears on reboot, so warn on timeout rather than Abort.
  StrCpy $R1 0
  deleted_loop_${UID}:
    nsExec::Exec 'cmd /c sc query ${EVERYTHING_SERVICE}'
    Pop $0
    ${If} $0 == 1060
      Goto deleted_done_${UID}
    ${EndIf}
    IntOp $R1 $R1 + 1
    ${If} $R1 > 20
      DetailPrint "Warning: ${EVERYTHING_SERVICE} still present after 10s; it will be removed on reboot"
      Goto deleted_done_${UID}
    ${EndIf}
    Sleep 500
    Goto deleted_loop_${UID}
  deleted_done_${UID}:
!macroend

!macro CLEANUP_OLD_PERUSER
  ; Delete the previous per-user install across ALL profiles — FILE DELETION
  ; ONLY, never running the old (user-writable) uninstaller (§5 / D5). The old
  ; install dir is removed only when it actually holds our Everything.exe, so an
  ; unrelated app that happens to be named "Everything" is left untouched.
  ; PRESERVE each profile's .pathignore/.pathindexing: every user's first GUI run
  ; migrates those sidecars into the machine config (§5), so only the obsolete
  ; per-user index DB (the large derived data) is removed here — RMDir'ing the
  ; whole roaming folder would run ahead of that migration and permanently lose
  ; the user's rules. Junction profiles (All Users / Default User) resolve to
  ; paths where these subdirectories don't exist, so the calls are no-ops there.
  FindFirst $R2 $R3 "$PROFILESDIR\*"
  cleanup_loop:
    StrCmp $R3 "" cleanup_done
    StrCmp $R3 "." cleanup_next
    StrCmp $R3 ".." cleanup_next
    Delete "$PROFILESDIR\$R3\AppData\Roaming\com.everything.app\index.db"
    Delete "$PROFILESDIR\$R3\AppData\Roaming\com.everything.app\index.db-wal"
    Delete "$PROFILESDIR\$R3\AppData\Roaming\com.everything.app\index.db-shm"
    ${If} ${FileExists} "$PROFILESDIR\$R3\AppData\Local\Everything\Everything.exe"
      RMDir /r "$PROFILESDIR\$R3\AppData\Local\Everything"
    ${EndIf}
  cleanup_next:
    FindNext $R2 $R3
    Goto cleanup_loop
  cleanup_done:
  FindClose $R2
!macroend

!macro KILL_OUR_PROCESSES_BY_PATH UID
  ; Path-scoped termination (§3.2): stop only Everything.exe instances whose
  ; module path is under OUR locations ($INSTDIR or an old per-user install), so
  ; voidtools' unrelated Everything.exe is never touched. The native template
  ; performs no process-name-only check. Failure leaves files locked and makes
  ; the ordinary installer copy fail rather than killing an unrelated app.
  FileOpen $9 "$TEMP\everything_killpath.ps1" w
  StrCmp $9 "" kill_skip_${UID} 0
    FileWrite $9 "Get-Process Everything -ErrorAction SilentlyContinue | Where-Object { $$_.Path -and ($$_.Path -like '$INSTDIR\*' -or $$_.Path -like '*\AppData\Local\Everything\Everything.exe') } | Stop-Process -Force -ErrorAction SilentlyContinue$\r$\n"
    FileClose $9
    nsExec::Exec 'powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$TEMP\everything_killpath.ps1"'
    Pop $0
    Delete "$TEMP\everything_killpath.ps1"
  kill_skip_${UID}:
!macroend

; ---------------------------------------------------------------------------
; Install
; ---------------------------------------------------------------------------

!macro NSIS_HOOK_PREINSTALL
  InitPluginsDir
  File /oname=$PLUGINSDIR\prepare-upgrade.ps1 "${__FILEDIR__}\prepare-upgrade.ps1"
  DetailPrint "Stopping EverythingIndexer before replacing files..."
  nsExec::ExecToLog /TIMEOUT=60000 '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\prepare-upgrade.ps1" -InstallDir "$INSTDIR" -StateFile "$PLUGINSDIR\upgrade-state.json"'
  Pop $0
  ${If} $0 != 0
    Abort "Could not stop EverythingIndexer safely (exit=$0). See installation details."
  ${EndIf}
  SetShellVarContext all
!macroend

!macro NSIS_HOOK_RESTORE_INDEXER
  ${If} $PLUGINSDIR != ""
    ${If} ${FileExists} "$PLUGINSDIR\upgrade-state.json"
      DetailPrint "Restoring the previous EverythingIndexer startup setting..."
      nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\prepare-upgrade.ps1" -Mode Restore -StateFile "$PLUGINSDIR\upgrade-state.json"'
      Pop $0
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  SetShellVarContext all

  ; -- 0) Data root: create + own (SYSTEM) + ACL (D1). Fatal. --
  ;    data\   : Users read-only (RX)  -> DB/markers can't be tampered with.
  ;    config\ : Users Modify (M)      -> GUI edits sidecars unelevated.
  ;    Re-setting owner + inheritance also neutralizes a pre-planted directory.
  CreateDirectory "${EVERYTHING_DATA_ROOT}"
  CreateDirectory "${EVERYTHING_DATA_ROOT}\data"
  CreateDirectory "${EVERYTHING_DATA_ROOT}\config"
  nsExec::Exec 'cmd /c icacls "${EVERYTHING_DATA_ROOT}" /setowner "NT AUTHORITY\SYSTEM" /T /C'
  Pop $0
  ${If} $0 != 0
    Abort "Failed to set owner on data root (icacls /setowner exit=$0)"
  ${EndIf}
  nsExec::Exec 'cmd /c icacls "${EVERYTHING_DATA_ROOT}" /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F" "BUILTIN\Administrators:(OI)(CI)F"'
  Pop $0
  ${If} $0 != 0
    Abort "Failed to set ACL on data root (icacls exit=$0)"
  ${EndIf}
  nsExec::Exec 'cmd /c icacls "${EVERYTHING_DATA_ROOT}\data" /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F" "BUILTIN\Administrators:(OI)(CI)F" "BUILTIN\Users:(OI)(CI)RX"'
  Pop $0
  ${If} $0 != 0
    Abort "Failed to set ACL on data dir (icacls exit=$0)"
  ${EndIf}
  nsExec::Exec 'cmd /c icacls "${EVERYTHING_DATA_ROOT}\config" /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F" "BUILTIN\Administrators:(OI)(CI)F" "BUILTIN\Users:(OI)(CI)M"'
  Pop $0
  ${If} $0 != 0
    Abort "Failed to set ACL on config dir (icacls exit=$0)"
  ${EndIf}

  ; -- 1) Register the service. Fatal. --
  nsExec::Exec 'cmd /c sc create ${EVERYTHING_SERVICE} binPath= "\"$INSTDIR\Everything.exe\" --service" start= delayed-auto DisplayName= "Everything Indexer"'
  Pop $0
  ${If} $0 != 0
    ; Already exists (upgrade) — update the binPath instead.
    nsExec::Exec 'cmd /c sc config ${EVERYTHING_SERVICE} binPath= "\"$INSTDIR\Everything.exe\" --service" start= delayed-auto'
    Pop $0
    ${If} $0 != 0
      Abort "Service registration failed (sc create/config exit=$0)"
    ${EndIf}
  ${EndIf}

  ; -- 2) Recovery policy BEFORE start, so a first-start crash is also covered.
  ;       Non-fatal (hardening). --
  nsExec::Exec 'cmd /c sc failure ${EVERYTHING_SERVICE} reset= 86400 actions= restart/5000/restart/30000/restart/60000'
  Pop $0
  ${If} $0 != 0
    DetailPrint "Warning: could not set service recovery policy (exit=$0)"
  ${EndIf}
  nsExec::Exec 'cmd /c sc failureflag ${EVERYTHING_SERVICE} 1'
  Pop $0
  ${If} $0 != 0
    DetailPrint "Warning: could not set service failureflag (exit=$0)"
  ${EndIf}

  ; -- 3) Start + confirm RUNNING. Fatal. --
  nsExec::Exec 'cmd /c sc start ${EVERYTHING_SERVICE}'
  Pop $0
  ${If} $0 != 0
  ${AndIf} $0 != 1056
    Abort "Service start failed (sc start exit=$0)"
  ${EndIf}
  !insertmacro WAIT_SERVICE_RUNNING "post"

  ; -- 4) Success-only cleanup of the previous per-user install (file deletion,
  ;       NO execution — the old per-user uninstaller is never run; §5/D5).
  ;       Sweeps every profile: old data (unique identifier) + the old per-user
  ;       install dir (guarded by our Everything.exe). HKCU keys/shortcuts are
  ;       cleaned by each user's first GUI run (§5 ②). Our own running processes
  ;       are already path-scope-killed in PRE(UN)INSTALL. --
  !insertmacro CLEANUP_OLD_PERUSER
  Delete "$PLUGINSDIR\upgrade-state.json"
!macroend

; ---------------------------------------------------------------------------
; Uninstall
; ---------------------------------------------------------------------------

!macro NSIS_HOOK_PREUNINSTALL
  SetShellVarContext all
  ; Path-scope-kill our own running processes so their files unlock.
  !insertmacro KILL_OUR_PROCESSES_BY_PATH "preun"
  nsExec::Exec 'cmd /c sc stop ${EVERYTHING_SERVICE}'
  Pop $0
  !insertmacro WAIT_SERVICE_STOPPED "preun"
  nsExec::Exec 'cmd /c sc delete ${EVERYTHING_SERVICE}'
  Pop $0
  ${If} $0 != 0
    DetailPrint "Warning: service delete failed (exit=$0)"
  ${EndIf}
  !insertmacro WAIT_SERVICE_DELETED "preun"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  SetShellVarContext all
  ; Q1 (v4 clarification): remove derived data (data\ = index/cursor/logs) only;
  ; PRESERVE user rules in config\ so a reinstall keeps them. /REBOOTOK handles
  ; files still open (e.g. a lingering handle).
  RMDir /r /REBOOTOK "${EVERYTHING_DATA_ROOT}\data"
  ; Remove the app root only if empty — config\ present means it is kept.
  RMDir "${EVERYTHING_DATA_ROOT}"
!macroend
