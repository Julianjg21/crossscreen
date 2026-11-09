# Prueba entre dos equipos Windows

Esta es una versión **alfa de prueba**. El objetivo es validar la cadena completa sobre una red real, por cable y por Wi-Fi.

| Equipo | Papel | Necesita |
|---|---|---|
| **PC-A (fuente)** | Su escritorio se **extiende** con un monitor virtual | Driver de monitor virtual instalado (ver más abajo) + `crossscreen-desktop.exe` (o `crossscreen.exe` por CLI) |
| **PC-B (receptor)** | Hace de **pantalla** | `crossscreen-desktop.exe`, `crossscreen-agent.exe` y `crossscreen-viewer.exe` (sin driver ni permisos de administrador) |

**Requisitos de ambos equipos:**
- Windows 10 21H2+ u 11, x64.
- La misma red local.
- GPU con decodificación H.264 por hardware en el receptor (cualquier Intel, AMD o NVIDIA de los últimos ~10 años).

## 0. Descargar e instalar
**Recomendado: instalador.** En los dos equipos, descarga `CrossScreen_<versión>_x64-setup.exe` desde **Releases** y ejecútalo (pide permiso de administrador). Instala el panel, el agente y el visor en *Archivos de programa*, crea el acceso directo en el menú Inicio y el desinstalador, y **añade las reglas del firewall** para que el equipo pueda recibir pantalla en redes **privadas** (en redes públicas no abre nada). Para actualizar, ejecuta el instalador nuevo encima.

**Alternativa portable:** `crossscreen-<versión>-windows-x64.zip`, descomprimido por ejemplo en `C:\CrossScreen`. Sin instalar nada; la primera vez que el equipo reciba pantalla, Windows preguntará por el firewall: pulsa **Permitir** (redes privadas).

Se puede usar la **aplicación gráfica** (panel CrossScreen) o **PowerShell** en la carpeta de instalación.

> Los ejecutables no están firmados todavía, así que Windows SmartScreen puede avisar. Para comprobar que la descarga está íntegra, compara los hashes con `SHA256SUMS.txt`:
> `Get-FileHash .\crossscreen.exe`

## 1. Driver de monitor virtual (solo PC-A, una vez)
Si PC-A es el portátil donde ya se instaló durante la PoC-1, **sáltate este paso** (`.\crossscreen.exe display list` debe mostrar un `[virtual] VDD by MTT`).

En otro equipo:
1. Descarga `VirtualDisplayDriver-x86.Driver.Only.zip` de la release **25.7.23** de <https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/tag/25.7.23> y descomprímelo.
2. En una PowerShell **como administrador**:
   ```powershell
   .\crossscreen-helper.exe vdd-install --from <carpeta>\VirtualDisplayDriver
   ```
   El helper comprueba el SHA-256 de cada archivo contra `vdd-manifest.toml` y rechaza cualquier otra versión.
3. Comprueba con `.\crossscreen.exe display list` (ya sin administrador).

Para desinstalar: `.\crossscreen-helper.exe vdd-uninstall --purge-config` (como administrador).

## 2. Averiguar la IP de PC-B
En PC-B: `ipconfig`. Anota la **dirección IPv4** del adaptador que estés usando (Ethernet o Wi-Fi), por ejemplo `192.168.1.50`.

## 3. Emparejar (una vez)
1. **PC-B:**
   ```powershell
   .\crossscreen.exe pair-listen --bind 0.0.0.0:47901
   ```
   - Muestra un **PIN de 6 dígitos**, válido 2 minutos.
   - Si aparece el aviso del **Firewall de Windows**, permite el acceso en **redes privadas**.
2. **PC-A:**
   ```powershell
   .\crossscreen.exe pair 192.168.1.50:47901 <PIN>
   ```
3. Los dos muestran "Emparejado con …" y la huella corta del otro. **Comprueba que las huellas coinciden** con lo que muestra `.\crossscreen.exe id` en cada equipo.

## 4. Transmitir

### Opción A: Desde el panel gráfico (`crossscreen-desktop.exe`, sin consola)
1. **En PC-B (receptor):** abre `crossscreen-desktop.exe` con doble clic, ve a **«Transmitir pantalla»** y pulsa **«Activar modo pantalla»** (en «Recibir en este equipo»).
2. **En PC-A (fuente):** abre `crossscreen-desktop.exe` con doble clic, ve a **«Red local»** o **«Transmitir pantalla»** (la IP y el puerto se autocompletan por mDNS) y pulsa **«Iniciar transmisión»**.

### Opción B: Por línea de comandos (PowerShell)
1. **PC-B (receptor):**
   ```powershell
   .\crossscreen-viewer.exe --listen 0.0.0.0:47810 --fullscreen
   ```
   - Permite el acceso en el aviso del firewall si aparece.
   - Para probar en ventana, quita `--fullscreen` (por defecto se abre a 1920×1080; usa `--size 1280x720` para otro tamaño).
   - Para salir: cierra la ventana o pulsa Ctrl+C en la consola.
2. **PC-A (fuente):**
   ```powershell
   .\crossscreen.exe peers                       # anota el prefijo de la huella de PC-B
   .\crossscreen.exe stream 192.168.1.50:47810 --peer <prefijo> --seconds 120
   ```
   - Aparece un **monitor nuevo a la derecha** del escritorio de PC-A, y PC-B lo muestra.
   - Mueve ventanas y el ratón hacia ese monitor.
   - Al terminar (`--seconds`, Ctrl+C o cierre del visor), el monitor virtual **desaparece solo**.
   - Opciones útiles: `--width 2560 --height 1440 --hz 60`, `--mbps 30`, `--mode duplicate` (duplica la pantalla principal en vez de extender, sin driver).

### Durante la transmisión: qué probar
- **Monitor extendido:** arrastra ventanas al monitor nuevo de PC-A y comprueba que se ven en PC-B.
- **Ratón y teclado desde PC-B (F6):** **desactivado por defecto** (por defecto solo se transmite la imagen). Para permitirlo, en PC-A marca «Permitir que el receptor controle este equipo» en el panel o usa `--allow-remote-input` en `stream`. Entonces, con la ventana del visor enfocada en PC-B, el ratón y el teclado de PC-B controlan el monitor virtual de PC-A.
- **Portapapeles (F7), en ambos sentidos.** Está **desactivado por defecto** (el portapapeles puede contener contraseñas). Hay que activarlo **en los dos equipos**:
  - en el panel, con la casilla "Sincronizar el portapapeles de texto" al transmitir y al recibir;
  - por línea de comandos, añadiendo `--clipboard` al visor (PC-B) y a `stream` (PC-A).

  Después:
  1. Copia un texto en PC-A (Ctrl+C) y pégalo en PC-B (Ctrl+V). Debería llegar en menos de 1 s.
  2. Copia otro texto en PC-B y pégalo en PC-A.
  3. Prueba saltos de línea, tildes y emojis (👍🏽).

  Solo se sincroniza **texto**, de hasta 1 MiB, y solo lo que copies **después** de conectar: lo que ya estaba copiado no se envía.

## 5. Qué anotar y enviarme
Al terminar, **cada lado imprime un resumen**. Cópialo de los dos equipos, indicando:
- si era **cable o Wi-Fi** (y qué Wi-Fi: 5 GHz/6 GHz, distancia aproximada al router);
- la resolución usada;
- lo que viste: fluidez, cortes, artefactos, si el cursor seguía bien al ratón.

> ⚠️ La línea "captura→Present (MISMO EQUIPO)" **solo es válida con los dos programas en el mismo PC**. Entre dos equipos los relojes son distintos y ese número no significa nada; el visor no la muestra salvo con `--same-host-latency`.

**Prueba de red pura** (opcional, sin vídeo real; mide pérdidas y latencia del transporte):
```powershell
# PC-B
.\crossscreen.exe bench-serve --bind 0.0.0.0:47800
# PC-A (repetir con --pacing off y con --pacing 0.6)
.\crossscreen.exe bench 192.168.1.50:47800 --peer <prefijo> --mbps 20 --seconds 10
.\crossscreen.exe bench 192.168.1.50:47800 --peer <prefijo> --mbps 50 --seconds 10
```

## Audio (opcional)

Por defecto solo se transmite la imagen. Para oír en el receptor lo que suena en el emisor:
- **Emisor:** marca «Enviar el audio de este equipo» (o `--audio` en `crossscreen stream`).
- **Receptor:** marca «Reproducir el audio que envíe el emisor» en «Recibir pantalla» (o `--audio` en `crossscreen-viewer`).

Hacen falta las dos casillas. Se envía todo el audio del sistema del emisor (Opus, 48 kHz estéreo, ~130 kbps). El receptor amortigua la variación de la red con un búfer de 60–160 ms que se ajusta solo y compensa la pequeña diferencia de reloj entre tarjetas de sonido. El resumen del visor indica tramas reproducidas, pérdidas ocultadas y vacíos del búfer.

## Encender y usar (arranque automático)

En el panel, **Este equipo → Inicio**:
- **Iniciar CrossScreen al iniciar sesión**: el agente arranca en segundo plano, sin ventana.
- **Quedar listo para recibir pantalla al iniciar**: abre el visor receptor (pantalla completa, puerto 47810) en cuanto arranca el agente.

Con las dos activadas, el equipo funciona como pantalla nada más encenderlo: el otro equipo puede transmitirle sin abrir el panel. Se desactivan desde el mismo sitio. En Linux no está disponible con el AppImage (usa el `.deb`).

## Registros (logs) para diagnosticar

Cada programa de CrossScreen (panel, agente, visor y `crossscreen`) guarda un archivo por ejecución en:

- **Linux:** `~/.local/state/crossscreen/logs/`
- **Windows:** `%LOCALAPPDATA%\CrossScreen\logs\`

Los nombres son `<programa>-<fecha UTC>-<pid>.log` (p. ej. `viewer-20261109-042150-24812.log`). Se guardan los 10 últimos de cada programa. En el panel, el botón **«Abrir carpeta de registros»** abre esa carpeta. Si algo falla, mándame los últimos `viewer-*`, `agent-*` y `cli-*` **de los dos equipos**.

## Problemas frecuentes
| Síntoma | Causa probable | Solución |
|---|---|---|
| `pair` o `stream` no conectan | Firewall de PC-B, o red marcada como "Pública" | Permitir la app en el aviso del firewall; marcar la red como **Privada** |
| Conecta por cable pero no por Wi-Fi | El router aísla a los clientes Wi-Fi entre sí (*AP isolation*) | Desactivar el aislamiento o usar cable |
| "la sesión está bloqueada o no es la consola activa" en PC-A | La pantalla de PC-A está bloqueada | Desbloquear PC-A |
| "no hay ningún monitor virtual disponible" | El driver no está instalado, o está en error (código 43) | Paso 1, o `crossscreen-helper vdd-restart` como administrador |
| "ningún equipo emparejado coincide" | El prefijo `--peer` no corresponde | `crossscreen peers` y copiar el prefijo correcto |
| Imagen congelada unos segundos | Pérdidas en Wi-Fi | Anotarlo (es justo lo que queremos medir) |
