# Prueba entre dos equipos Windows

Esta es una versión **alfa de prueba**. El objetivo es validar la cadena completa sobre una red real, por cable y por Wi-Fi.

| Equipo | Papel | Necesita |
|---|---|---|
| **PC-A (fuente)** | Su escritorio se **extiende** con un monitor virtual | Driver de monitor virtual instalado (ver más abajo) + `crossscreen.exe` |
| **PC-B (receptor)** | Hace de **pantalla** | Solo `crossscreen.exe` y `crossscreen-viewer.exe`. Sin driver ni permisos de administrador |

**Requisitos de ambos equipos:**
- Windows 10 21H2+ u 11, x64.
- La misma red local.
- GPU con decodificación H.264 por hardware en el receptor (cualquier Intel, AMD o NVIDIA de los últimos ~10 años).

## 0. Descargar
1. En los dos equipos, descarga `crossscreen-<versión>-windows-x64.zip` desde la sección **Releases** del repositorio.
2. Descomprímelo, por ejemplo en `C:\CrossScreen`.
3. Abre **PowerShell** en esa carpeta.

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

## Problemas frecuentes
| Síntoma | Causa probable | Solución |
|---|---|---|
| `pair` o `stream` no conectan | Firewall de PC-B, o red marcada como "Pública" | Permitir la app en el aviso del firewall; marcar la red como **Privada** |
| Conecta por cable pero no por Wi-Fi | El router aísla a los clientes Wi-Fi entre sí (*AP isolation*) | Desactivar el aislamiento o usar cable |
| "la sesión está bloqueada o no es la consola activa" en PC-A | La pantalla de PC-A está bloqueada | Desbloquear PC-A |
| "no hay ningún monitor virtual disponible" | El driver no está instalado, o está en error (código 43) | Paso 1, o `crossscreen-helper vdd-restart` como administrador |
| "ningún equipo emparejado coincide" | El prefijo `--peer` no corresponde | `crossscreen peers` y copiar el prefijo correcto |
| Imagen congelada unos segundos | Pérdidas en Wi-Fi | Anotarlo (es justo lo que queremos medir) |
