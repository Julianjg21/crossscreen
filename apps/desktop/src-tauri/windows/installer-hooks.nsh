; Ganchos del instalador NSIS de CrossScreen (Tauri los incluye con
; bundle.windows.nsis.installerHooks). El instalador es "perMachine" y corre
; elevado, lo que permite crear las reglas del firewall.
;
; Reglas: entrada UDP (QUIC, mDNS y emparejamiento) solo en redes privadas y
; de dominio, por programa (cualquier puerto). En redes públicas no se abre
; nada: allí Windows seguirá preguntando.

!macro CS_STOP_PROCESSES
  ; Archivos en uso no se pueden sustituir: cerrar agente y visor.
  nsExec::ExecToLog 'taskkill /F /IM crossscreen-agent.exe'
  Pop $0
  nsExec::ExecToLog 'taskkill /F /IM crossscreen-viewer.exe'
  Pop $0
!macroend

!macro CS_DELETE_FIREWALL_RULES
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="CrossScreen - visor"'
  Pop $0
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="CrossScreen - agente"'
  Pop $0
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="CrossScreen - consola"'
  Pop $0
!macroend

!macro CS_ADD_FIREWALL_RULE NAME EXE
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="${NAME}" dir=in action=allow protocol=UDP profile=private,domain program="$INSTDIR\${EXE}" enable=yes description="CrossScreen: recibir pantalla, emparejar y descubrir equipos en la red local"'
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro CS_STOP_PROCESSES
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Idempotente: quitar las de una instalación anterior y crearlas de nuevo
  ; apuntando a la ruta actual.
  !insertmacro CS_DELETE_FIREWALL_RULES
  !insertmacro CS_ADD_FIREWALL_RULE "CrossScreen - visor" "crossscreen-viewer.exe"
  !insertmacro CS_ADD_FIREWALL_RULE "CrossScreen - agente" "crossscreen-agent.exe"
  !insertmacro CS_ADD_FIREWALL_RULE "CrossScreen - consola" "crossscreen.exe"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro CS_STOP_PROCESSES
  !insertmacro CS_DELETE_FIREWALL_RULES
  ; Inicio automático (lo crea el agente en la clave del usuario).
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "CrossScreen"
!macroend
