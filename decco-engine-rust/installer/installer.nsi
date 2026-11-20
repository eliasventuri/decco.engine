; Decco Engine (Rust) - NSIS Always-Silent Installer

!ifndef APP_VERSION
  !define APP_VERSION "1.1.82"
!endif

!ifndef EXE_PATH
  !define EXE_PATH "..\target\release\decco-engine.exe"
!endif

!ifndef OUT_FILE
  !define OUT_FILE "..\target\release\decco-engine-Setup-${APP_VERSION}.exe"
!endif

!define APP_NAME "Decco Engine"
!define EXE_NAME "decco-engine.exe"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeccoEngine"

Name "${APP_NAME} ${APP_VERSION}"
OutFile "${OUT_FILE}"
Icon "icon.ico"
UninstallIcon "icon.ico"
InstallDir "$LOCALAPPDATA\Programs\Decco Engine"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
SetCompressor /SOLID lzma

Function .onInit
  ; Close running instances (both Rust and legacy Electron) before installing/updating
  nsExec::Exec 'taskkill /F /IM "${EXE_NAME}" /T'
  nsExec::Exec 'taskkill /F /IM "Decco Engine.exe" /T'
  Sleep 500
FunctionEnd

Section "Decco Engine" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "/oname=${EXE_NAME}" "${EXE_PATH}"
  File "icon.ico"
  File "..\..\icon.png"

  ; Write uninstaller
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Register Add/Remove Programs (per-user, no UAC needed for silent updates)
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Decco Team"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\icon.ico"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

  ; Register decco:// protocol handler (HKCU\Software\Classes so silent updates need no elevation)
  DeleteRegKey HKCU "Software\Classes\decco"
  WriteRegStr HKCU "Software\Classes\decco" "" "URL:Decco Protocol"
  WriteRegStr HKCU "Software\Classes\decco" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\decco\DefaultIcon" "" "$INSTDIR\icon.ico"
  WriteRegStr HKCU "Software\Classes\decco\shell" "" ""
  WriteRegStr HKCU "Software\Classes\decco\shell\open" "" ""
  WriteRegStr HKCU "Software\Classes\decco\shell\open\command" "" '"$INSTDIR\${EXE_NAME}" "%1"'

  ; Register auto-start on Windows login
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeccoEngine" '"$INSTDIR\${EXE_NAME}" --hidden'

  ; Start Menu shortcut
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${EXE_NAME}" "" "$INSTDIR\icon.ico"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\Desinstalar ${APP_NAME}.lnk" "$INSTDIR\Uninstall.exe" "" "$INSTDIR\icon.ico"

  ; Launch Decco Engine after install / silent update
  Exec '"$INSTDIR\${EXE_NAME}" --hidden'
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /F /IM "${EXE_NAME}" /T'
  Sleep 400

  Delete "$INSTDIR\${EXE_NAME}"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\icon.png"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\Desinstalar ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeccoEngine"
  DeleteRegKey HKCU "Software\Classes\decco"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
