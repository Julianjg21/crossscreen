# Ubuntu como pantalla (receptor Linux): Windows → Linux

Tu PC con Windows (PC-A) **extiende su escritorio** y el equipo Linux (PC-B) lo muestra en pantalla. Versión **alfa**.

## Estado del receptor Linux
| Función | Estado |
|---|---|
| Recibir, decodificar y mostrar el monitor extendido | ✅ (OpenH264 por software + wgpu/Vulkan) |
| Ratón y teclado del Linux controlando el monitor extendido | ✅ (mismo código que en Windows; pendiente de probar en tu Ubuntu) |
| Cursor de la fuente dibujado encima del vídeo | ✅ (aparece cuando llega su primera forma) |
| Decodificación por hardware (VAAPI) | ❌ pendiente: por ahora decodifica la CPU |
| Portapapeles de texto en Linux (ambos sentidos) | ✅ opcional (casilla del panel o `--clipboard`); X11/XWayland |
| Linux como fuente: extender (monitor virtual de GNOME) y duplicar | ✅ GNOME en Wayland |
| Ratón y teclado de Windows controlando Ubuntu (Linux como fuente) | ✅ extender: sin diálogo; duplicar: se pide en el mismo diálogo de compartir pantalla |

## Linux como fuente: Ubuntu → Windows (experimental)

Dos modos:
- **Extender** (por defecto): GNOME crea un **monitor virtual** con la resolución de la pantalla de Windows y Windows lo muestra. Sin diálogo. Necesita GNOME en **Wayland** (lo normal en Ubuntu 22.04/24.04; en la pantalla de inicio de sesión no elijas «Ubuntu en Xorg»). El monitor aparece en *Configuración → Pantallas*, donde puedes colocarlo a la izquierda o a la derecha; al terminar desaparece solo.
- **Duplicar**: Windows muestra la pantalla que elijas en el diálogo de GNOME.

Requisitos: los dos equipos emparejados y OpenH264 instalado en Ubuntu (`./fetch-openh264.sh`, ver paso 3; con el `.deb` está en `/usr/lib/CrossScreen/fetch-openh264.sh`).

1. **En Windows (receptor):** en el panel, «Recibir pantalla»; o por consola `crossscreen-viewer.exe --listen 0.0.0.0:47810`.
   **La primera vez que ese PC recibe**, Windows muestra el aviso del firewall para `crossscreen-viewer`: pulsa **Permitir** (redes privadas). Si lo bloqueas, Ubuntu no podrá conectar («no se pudo conectar… comprueba el firewall»).
2. **En Ubuntu (emisor):** en el panel, «Transmitir pantalla» → el equipo Windows → «Iniciar transmisión»; o por consola:
   ```bash
   ./crossscreen stream <IP_DEL_WINDOWS>:47810 --peer <HUELLA_DEL_WINDOWS>
   ```
3. Solo al **duplicar**: GNOME abre su diálogo; elige la pantalla y pulsa **Compartir**. Tienes hasta 2 minutos; la conexión con Windows se abre **después** de aceptar, así que no hay prisa.
4. Para terminar: «Detener transmisión» en el panel, Ctrl+C en la consola, cerrar la ventana del visor en Windows o «Dejar de compartir» en la barra superior de GNOME. Al terminar, GNOME apaga su indicador de pantalla compartida.

Cómo funciona y qué falta:
- Con la pantalla quieta GNOME no envía imágenes nuevas; CrossScreen reenvía la última una vez por segundo, así que la imagen aparece al instante aunque nada se mueva.
- Codificación por CPU (OpenH264): en las pruebas, ~10 ms por frame a 1080p.
- **Ratón y teclado desde Windows:** hay que **permitirlo** al iniciar (por defecto solo se transmite la imagen): casilla «Permitir que el receptor controle este equipo» del panel o `--allow-remote-input` en la consola. Con eso, al mover el ratón sobre la ventana del visor en Windows, controlas Ubuntu (en extender, el monitor virtual; en duplicar, la pantalla compartida). Al **duplicar**, el diálogo de GNOME pide además permitir el control remoto: acéptalo. Las teclas van por posición física (como un teclado conectado a Ubuntu), así que se respeta la distribución configurada en Ubuntu.
- **Portapapeles:** marca «Sincronizar el portapapeles» en **los dos** equipos. Solo texto. Lo copiado antes de empezar no se envía.

## 1. Requisitos (Ubuntu 24.04, x86_64)
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

## 3. Descargar el códec de Cisco (una vez)
**Con el panel:** en «Transmitir o recibir pantalla» aparece el aviso «Falta el códec H.264»; pulsa **Descargar e instalar OpenH264**.

**Por consola** (paquete `.tar.gz`; con el `.deb` el script está en `/usr/lib/CrossScreen/`):
```bash
./fetch-openh264.sh
```
Cualquiera de los dos descarga `libopenh264-2.6.0-linux64.8.so` del servidor oficial de Cisco, verifica su SHA-256 y lo deja en `~/.local/share/crossscreen/`. No va incluido en el paquete por la licencia de patentes de H.264: al usar el binario de Cisco, la licencia la cubre Cisco.

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

## Varios monitores (opcional)

Por defecto se transmite **un** monitor. Para extender el escritorio con varios a la vez (por ejemplo, las dos pantallas de un sobremesa):
- **Receptor:** en «Recibir pantalla», elige «Pantallas a recibir: 2» (o 3, 4). Abre un visor por pantalla en el puerto elegido y los siguientes (47810, 47811…), cada uno en un monitor de ese equipo; si tiene menos monitores, los que sobran van en ventana.
- **Emisor:** en «Transmitir», modo «Extender» y «Monitores virtuales: 2». Cada monitor virtual va al puerto siguiente del receptor.
- En la CLI: `crossscreen stream IP:47810 --peer … --monitors 2` y un `crossscreen-viewer --listen 0.0.0.0:47810` / `:47811` por pantalla.

El portapapeles y el audio viajan solo con el primer monitor; el ratón y el teclado funcionan en todos. En Windows, el driver de monitor virtual debe tener al menos tantos monitores como pidas: se ajusta una vez en su configuración (`monitors` → `count`, con permisos de administrador). Si no hay bastantes, el panel lo dice antes de conectar. En Linux (GNOME) no hace falta configurar nada.

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

Cada programa (panel, agente, visor y `crossscreen`) guarda un archivo por ejecución en:

- **Linux:** `~/.local/state/crossscreen/logs/`
- **Windows:** `%LOCALAPPDATA%\CrossScreen\logs\`

Los nombres son `<programa>-<fecha UTC>-<pid>.log` (p. ej. `viewer-20261109-042150-24812.log`). Se guardan los 10 últimos de cada programa. En el panel, el botón **«Abrir carpeta de registros»** abre esa carpeta. Si algo falla, mándame los últimos `viewer-*`, `agent-*` y `cli-*` **de los dos equipos**.

## Problemas frecuentes
| Síntoma | Causa probable | Solución |
|---|---|---|
| "no se encontró libopenh264…" | Falta el decodificador de Cisco | `./fetch-openh264.sh` |
| "no hay adaptador gráfico compatible" | Falta Vulkan | `sudo apt install libvulkan1 mesa-vulkan-drivers` |
| `pair` o `stream` no conectan | Cortafuegos | Abrir 47901/udp y 47810/udp en `ufw` |
| Imagen con saltos y "saltados por retraso en cola" alto | La CPU no decodifica a tiempo (OpenH264 por software) | Bajar la resolución del monitor virtual (`--width 1600 --height 900` en `stream`) mientras llega VAAPI |
