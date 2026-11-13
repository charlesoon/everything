; Native GPUI installer. Reuses the existing service/ACL migration hooks.
Unicode True
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "hooks.nsh"

Name "Everything"
OutFile "${OUTPUT}"
InstallDir "$PROGRAMFILES64\Everything"
InstallDirRegKey HKLM "Software\com.everything.app" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Icon "${ICON}"
UninstallIcon "${ICON}"
!define MUI_ABORTWARNING
!define MUI_CUSTOMFUNCTION_ABORT RestoreIndexerOnAbort
!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Korean"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "Everything requires 64-bit Windows."
    Abort
  ${EndIf}
  SetRegView 64
  SetShellVarContext all
FunctionEnd

Function RestoreIndexerOnAbort
  !insertmacro NSIS_HOOK_RESTORE_INDEXER
FunctionEnd

Function .onInstFailed
  !insertmacro NSIS_HOOK_RESTORE_INDEXER
FunctionEnd

Section "Everything" SEC_MAIN
  !insertmacro NSIS_HOOK_PREINSTALL
  SetOutPath "$INSTDIR"
  File /oname=Everything.exe "${BINARY}"
  File /oname=icon.ico "${ICON}"
  SetOutPath "$INSTDIR\licenses"
  File "${__FILEDIR__}\..\assets\fonts\inter-LICENSE.txt"
  File "${__FILEDIR__}\..\assets\fonts\jetbrains-mono-LICENSE.txt"
  SetOutPath "$INSTDIR"
  !insertmacro NSIS_HOOK_POSTINSTALL
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKLM "Software\com.everything.app" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "DisplayName" "Everything"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "Publisher" "Everything"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "DisplayIcon" "$INSTDIR\icon.ico"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app" "NoRepair" 1
  CreateDirectory "$SMPROGRAMS\Everything"
  CreateShortcut "$SMPROGRAMS\Everything\Everything.lnk" "$INSTDIR\Everything.exe"
  CreateShortcut "$SMPROGRAMS\Everything\Uninstall.lnk" "$INSTDIR\Uninstall.exe"
  CreateShortcut "$DESKTOP\Everything.lnk" "$INSTDIR\Everything.exe"
SectionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
FunctionEnd

Section "Uninstall"
  !insertmacro NSIS_HOOK_PREUNINSTALL
  Delete "$DESKTOP\Everything.lnk"
  Delete "$SMPROGRAMS\Everything\Everything.lnk"
  Delete "$SMPROGRAMS\Everything\Uninstall.lnk"
  RMDir "$SMPROGRAMS\Everything"
  Delete "$INSTDIR\Everything.exe"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\licenses\inter-LICENSE.txt"
  Delete "$INSTDIR\licenses\jetbrains-mono-LICENSE.txt"
  RMDir "$INSTDIR\licenses"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\com.everything.app"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.everything.app"
  !insertmacro NSIS_HOOK_POSTUNINSTALL
SectionEnd
