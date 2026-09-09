; NSIS installer hooks for xianyu-delivery-assistant.
; The browser (Chrome/Edge) respawns nmhost.exe every few seconds via the
; extension reconnect loop. Overwriting a running exe makes NSIS fail with
; "Error opening file for writing". Sequence here:
;   PREINSTALL : rename the native-messaging manifest away (respawns now fail),
;                kill nmhost + the app, wait briefly
;   POSTINSTALL: restore the manifest; the extension reconnects automatically
; Keep this file ASCII-only (NSIS parses it in the ANSI codepage).

!macro NSIS_HOOK_PREINSTALL
  DetailPrint "Stopping nmhost and app processes..."
  Rename "$APPDATA\com.local.xianyu.deliveryassistant\native\com.local.xianyu.deliveryassistant.json" "$APPDATA\com.local.xianyu.deliveryassistant\native\manifest.locked"
  nsExec::Exec 'taskkill /F /IM nmhost.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM xianyu-delivery-assistant.exe'
  Pop $0
  Delete "$APPDATA\com.local.xianyu.deliveryassistant\native\com.local.xianyu.deliveryassistant.json.bak"
  Sleep 1500
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Rename "$APPDATA\com.local.xianyu.deliveryassistant\native\manifest.locked" "$APPDATA\com.local.xianyu.deliveryassistant\native\com.local.xianyu.deliveryassistant.json"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /IM nmhost.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM xianyu-delivery-assistant.exe'
  Pop $0
  Delete "$APPDATA\com.local.xianyu.deliveryassistant\native\com.local.xianyu.deliveryassistant.json"
  Delete "$APPDATA\com.local.xianyu.deliveryassistant\native\manifest.locked"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; nothing to do after uninstall
!macroend
