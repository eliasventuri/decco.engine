!macro customInit
  nsExec::Exec 'taskkill /F /IM "Decco Engine.exe" /T'
!macroend

!macro customInstall
  ; Register decco:// protocol handler
  DetailPrint "Registering decco:// protocol..."
  DeleteRegKey HKCR "decco"
  WriteRegStr HKCR "decco" "" "URL:Decco Protocol"
  WriteRegStr HKCR "decco" "URL Protocol" ""
  WriteRegStr HKCR "decco\DefaultIcon" "" "$INSTDIR\Decco Engine.exe,0"
  WriteRegStr HKCR "decco\shell" "" ""
  WriteRegStr HKCR "decco\shell\open" "" ""
  WriteRegStr HKCR "decco\shell\open\command" "" '"$INSTDIR\Decco Engine.exe" "%1"'

  ; Register auto-start on Windows login (current user)
  DetailPrint "Registering auto-start..."
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeccoEngine" '"$INSTDIR\Decco Engine.exe" --hidden'
!macroend

!macro customUnInstall
  ; Clean up auto-start entry on uninstall
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeccoEngine"
  ; Clean up protocol handler
  DeleteRegKey HKCR "decco"
!macroend
