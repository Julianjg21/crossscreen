# Ubuntu como pantalla (receptor Linux): Windows → Linux

Tu PC con Windows (PC-A) **extiende su escritorio** y el equipo Linux (PC-B) lo muestra en pantalla. Versión **alfa**.

## Estado del receptor Linux
| Función | Estado |
|---|---|
| Recibir, decodificar y mostrar el monitor extendido | ✅ (OpenH264 por software + wgpu/Vulkan) |
| Ratón y teclado del Linux controlando el monitor extendido | ✅ (mismo código que en Windows; pendiente de probar en tu Ubuntu) |
| Cursor de la fuente dibujado encima del vídeo | ✅ (aparece cuando llega su primera forma) |
| Decodificación por hardware (VAAPI) | ❌ pendiente: por ahora decodifica la CPU |
| Portapapeles en Linux | ❌ pendiente: el visor avisa y sigue sin él |
| Linux como fuente (duplicar el escritorio hacia Windows) | 🟡 experimental: GNOME Wayland con Portal/PipeWire; modo `extend` pendiente |

## Linux como fuente experimental

La fuente Linux actual solo duplica el monitor elegido en el diálogo de GNOME.
El receptor Windows debe estar escuchando y ambos equipos deben estar
emparejados. En Ubuntu GNOME con PipeWire instalado:

```bash
crossscreen stream <IP_DEL_WINDOWS>:47810 --peer <HUELLA_DEL_WINDOWS> --mode duplicate
```

Al iniciar se debe aceptar la autorización de captura del portal. El binario
Linux necesita también `libopenh264-2.6.0-linux64.8.so`, instalado mediante
`fetch-openh264.sh` como en el modo receptor. Esta ruta es experimental: todavía
no se ha validado en hardware real desde este repositorio y no incluye `extend`,
portapapeles ni inyección de entrada Linux.

Validado también en un Ubuntu real con el flujo Windows → Ubuntu completo. La prueba confirma que el receptor funciona fuera de WSL2; las métricas detalladas de GPU, sesión gráfica y latencia quedan pendientes de documentar por separado.

## 1. Requisitos (Ubuntu 22.04 o 24.04, x86_64)
Ubuntu Desktop ya trae casi todo. Si falta algo:
```bash
sudo apt install libvulkan1 mesa-vulkan-drivers libxkbcommon0 libwayland-client0 curl bzip2
```
Para comprobar que Vulkan usa tu GPU (no `llvmpipe`): `sudo apt install vulkan-tools && vulkaninfo --summary`.

## 2. Descargar CrossScreen
Descarga `crossscreen-<versión>-linux-x64.tar.gz` desde **Releases** del repositorio y descomprímelo:
```bash
tar xzf crossscreen-*-linux-x64.tar.gz
cd crossscreen-*-linux-x64
sha256sum -c SHA256SUMS.txt
```

## 3. Descargar el decodificador de Cisco (una vez)
```bash
./fetch-openh264.sh
```
Descarga `libopenh264-2.6.0-linux64.8.so` del servidor oficial de Cisco, verifica su SHA-256 y lo deja en `~/.local/share/crossscreen/`. No va incluido en el paquete por la licencia de patentes de H.264: al usar el binario de Cisco, la licencia la cubre Cisco.

## 4. Emparejar con el PC Windows (una vez)
1. **En Linux** (muestra un PIN):
   ```bash
   ./crossscreen pair-listen --bind 0.0.0.0:47901
   ```
2. **En Windows (PC-A):**
   ```powershell
   .\crossscreen.exe pair <IP-del-Linux>:47901 <PIN>
   ```
   La IP del Linux se ve con `ip -4 addr` (la de tu Wi-Fi o Ethernet, p. ej. `192.168.1.x`).
3. Comprueba que las huellas que muestran ambos coinciden con `crossscreen id` de cada equipo.

Si usas cortafuegos (`ufw`), abre los puertos UDP: `sudo ufw allow 47901/udp` y `sudo ufw allow 47810/udp`.

## 5. Transmitir
1. **En Linux** (receptor):
   ```bash
   ./crossscreen-viewer --listen 0.0.0.0:47810 --fullscreen
   ```
   Para probar en ventana, quita `--fullscreen` (`--size 1280x720` para otro tamaño). Para salir: Esc, cerrar la ventana o Ctrl+C en la terminal.
2. **En Windows (PC-A)**, desde el panel ("Transmitir pantalla" → elige el equipo Linux) o por consola:
   ```powershell
   .\crossscreen.exe peers                         # prefijo de la huella del Linux
   .\crossscreen.exe stream <IP-del-Linux>:47810 --peer <prefijo> --seconds 120
   ```

## 6. Qué enviarme
Al terminar, el visor imprime un resumen. Mándamelo, junto con:
- tu GPU y lo que dice `vulkaninfo --summary` (deviceName);
- si es GNOME en Wayland o en X11 (`echo $XDG_SESSION_TYPE`);
- cable o Wi-Fi, y la resolución;
- lo que viste: fluidez, saltos, si el ratón y el teclado respondían bien.

Las líneas clave del resumen son "espera en cola antes de decodificar", "decode (OpenH264, CPU)", "reensamblado→Present" y "saltados por retraso en cola". Si la CPU no da abasto, el visor salta frames en vez de acumular retraso; el contador lo indica.

## Problemas frecuentes
| Síntoma | Causa probable | Solución |
|---|---|---|
| "no se encontró libopenh264…" | Falta el decodificador de Cisco | `./fetch-openh264.sh` |
| "no hay adaptador gráfico compatible" | Falta Vulkan | `sudo apt install libvulkan1 mesa-vulkan-drivers` |
| `pair` o `stream` no conectan | Cortafuegos | Abrir 47901/udp y 47810/udp en `ufw` |
| Imagen con saltos y "saltados por retraso en cola" alto | La CPU no decodifica a tiempo (OpenH264 por software) | Bajar la resolución del monitor virtual (`--width 1600 --height 900` en `stream`) mientras llega VAAPI |
