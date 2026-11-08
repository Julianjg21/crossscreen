# 3. Estrategia de monitores virtuales

Un **monitor virtual real** cumple estas condiciones:
- aparece en la configuración de pantalla del SO;
- tiene EDID, modos y frecuencia;
- puede extender el escritorio;
- las aplicaciones pueden moverse a él.

Capturar una ventana no cuenta. Este documento define cómo conseguirlo en cada plataforma.

Todos los caminos se exponen detrás del *trait* `VirtualDisplayProvider` (ver [02-arquitectura.md](02-arquitectura.md)). Cada backend declara sus capacidades: modos arbitrarios, cambio de modo en caliente, HDR, número máximo de monitores y si requiere privilegios. Cuando ningún backend es viable, se muestra un error explícito con instrucciones; **nunca se recurre en silencio al modo duplicar.**

---

## 3.1 Windows: Indirect Display Driver (IddCx)

### Fundamentos técnicos
- **Modelo:** IddCx es el modelo oficial de Microsoft para monitores "indirectos" (USB, red). El driver es **UMDF** (modo usuario), así que un fallo no provoca pantallazo azul.
- **Qué hace el driver:**
  - informa de un monitor con EDID o con una lista de modos;
  - recibe del sistema una ***swap chain*** con los frames ya compuestos por DWM.
- **Disponibilidad:** desde Windows 10 1607. Las versiones relevantes son IddCx **1.4** (Win10 2004), 1.5 (Win11 21H2: modos dinámicos) y 1.9–1.10 (Win11 22H2+: HDR y FP16).
- **Firma:** el driver debe estar firmado.
  - Para distribución general hace falta **firma de atestación** de Microsoft (Partner Center), que requiere un **certificado EV**.
  - WHQL es opcional.
  - En ARM64 no se acepta una firma que no sea de Microsoft (lo confirma el fallo `0x800B0109` que documenta TwinDesk).

### Opciones evaluadas

| Opción | Pros | Contras | Veredicto |
|---|---|---|---|
| **A. VirtualDrivers/Virtual-Display-Driver (VDD)**, instalado en tiempo de ejecución | MIT; ya firmado (SignPath) en x64; activo; usado en producción por TwinDesk; configurable por XML; HDR | Dependencia externa y su configuración (XML en `C:\VirtualDisplayDriver`); no firmado por Microsoft, así que no funciona en ARM64; los cambios de modo exigen reiniciar el dispositivo | **Fases 1–3** |
| **B. MolotovCherry/virtual-display-rs** (Rust, IPC por tubería con nombre) | MIT; Rust; IPC programática con JSON `Monitor{id,modes}` | Sin firma pública (ScreenExtend tuvo que recompilarlo y meter un certificado raíz autofirmado, algo **inaceptable**); IddCx 1.2 | Base de código para la opción C |
| **C. Driver IddCx propio** (C++ con WDK o Rust con `windows-drivers-rs`) | Control total: IPC propia, modos dinámicos (IddCx 1.5+), **acceso directo a la *swap chain*** (evita DXGI DD y ahorra 1–3 ms), selección del adaptador de renderizado en portátiles híbridos | Requiere certificado EV, proceso de atestación y mantenimiento de un driver | **Fase 8**, si la tracción del proyecto lo justifica |
| D. *Dummy plug* HDMI o EDID falso | Sin software | Hardware, no escala, un monitor por puerto | Descartado |
| E. Mirror drivers / WDDM completo | — | Mirror está obsoleto desde Win8; un WDDM completo es desproporcionado | Descartado |

### Diseño con VDD (opción A)
1. **Instalación (helper privilegiado, bajo demanda):**
   1. descargar una release **fijada por versión y SHA-256**;
   2. verificar la firma Authenticode del `.cat` y el sujeto esperado;
   3. instalar con `pnputil /add-driver` y crear el dispositivo.

   **No se registran tareas como SYSTEM** (diferencia con TwinDesk). El usuario aprueba la elevación una sola vez.
2. **Control (agente, sin privilegios):**
   1. escribir el XML de configuración con un **parser XML real**, no con regex;
   2. activar o desactivar el dispositivo. Esto sí requiere elevación, y es la razón para valorar la opción C.
3. **Captura del monitor virtual:**
   1. localizar su `HMONITOR` y su salida DXGI por el nombre del EDID o el identificador de dispositivo;
   2. capturarlo con **DXGI Desktop Duplication** (`DuplicateOutput1` con formatos BGRA y FP16), que mantiene la textura en GPU;
   3. si DD falla (por ejemplo, la salida está en otro adaptador), recurrir a **Windows.Graphics.Capture** con el crate `windows-capture` (MIT).
4. **Topología:** forzar el modo "Extender" y la posición relativa con `SetDisplayConfig` (CCD API), y restaurar la topología anterior al terminar.
5. **Una salvedad que se valida en la PoC-1:** si VDD no permite activar o desactivar monitores sin elevación, la estrategia es **dejarlo instalado con N monitores desactivados por topología** (CCD) en lugar de por PnP. Esa operación no necesita privilegios.

### Problemas específicos de Windows a resolver
- **Escritorio seguro (UAC, Ctrl+Alt+Supr, pantalla de bloqueo).** DXGI DD no puede capturarlo desde la sesión del usuario. Fase 1: mostrar un aviso "escritorio seguro activo" en el receptor; el monitor virtual sigue existiendo. Si se requiere en el futuro, habrá que capturar desde un servicio en `winlogon`, como hace Sunshine.
- **Portátiles híbridos (Optimus).** El monitor virtual puede renderizarse en la iGPU mientras NVENC está en la dGPU, lo que obliga a una copia entre adaptadores. Hay que detectarlo y elegir un codificador del mismo adaptador. Una solución completa requiere IddCx 1.4+ con selección de adaptador (opción C).
- **DPI y escalado.** Se respeta el escalado que el usuario configure en Windows para el monitor virtual. El receptor solo informa de su resolución nativa y de su DPI para sugerir un valor.

---

## 3.2 Linux: no existe una API única

En Linux "crear un monitor" depende de **quién controla KMS**. En Wayland es el compositor, y cada compositor tiene su propia API. En X11 es el servidor X junto con el driver. Por eso se implementan varios backends, elegidos según el entorno detectado.

### Matriz de backends

| Entorno | Backend | Mecanismo | Privilegios | Monitor real | Captura asociada | Prioridad |
|---|---|---|---|---|---|---|
| **GNOME (Mutter) Wayland** | `mutter-virtual` | D-Bus `org.gnome.Mutter.ScreenCast` → `CreateSession` → `RecordVirtual` | Ninguno (sesión del usuario) | ✅ Monitor lógico del compositor mientras viva el *stream* | PipeWire (DMA-BUF) del mismo *stream* | **F4: primero** |
| **KDE Plasma 6 Wayland** | `kwin-virtual` | Protocolo Wayland `zkde_screencast_unstable_v1` → `stream_virtual_output` (o el portal con tipo de fuente `VIRTUAL`) | Ninguno (KWin puede pedir autorización) | ✅ | PipeWire | F4 |
| **Sway / wlroots** | `wlroots-headless` | `swaymsg create_output` (salida *headless*); Hyprland: `hyprctl output create headless` | Ninguno | ✅ | `xdg-desktop-portal-wlr` / `ext-image-copy-capture-v1` → PipeWire | F4 |
| **Cualquier entorno (X11 o Wayland con soporte multi-GPU)** | `evdi` | Módulo del kernel **EVDI** (DisplayLink): crea un **conector DRM** de verdad, que el compositor o el servidor X ve como un monitor físico conectado | Cargar el módulo y abrir `/dev/dri/cardN` (regla udev) | ✅ A nivel del kernel | `libevdi` (`evdi_grab_pixels`, CPU) | F4b: *fallback* universal |
| **X11 con driver modesetting/intel** | `xrandr-virtual` | Salidas `VIRTUAL1` del driver intel o conector desconectado forzado | Variable; a veces root | ⚠️ Depende del driver | XShm / XDamage | Solo si EVDI no es viable |
| Pruebas / CI | `vkms` | Driver virtual KMS del kernel | root | Solo para pruebas | — | CI, no producción |

### Detalles por backend

**Mutter `RecordVirtual` (GNOME, el camino más limpio)**
- El agente abre una sesión de ScreenCast con `RecordVirtual(properties)`, incluyendo `cursor-mode = metadata` para recibir el cursor fuera de banda.
- **Comportamiento:**
  - Mutter crea un monitor virtual cuyo tamaño se negocia **con el formato del *stream* PipeWire** (el agente propone la resolución del receptor en los parámetros SPA).
  - El monitor desaparece al cerrar el *stream*.
- **Por qué es la opción preferida:**
  - no requiere instalar nada, ni privilegios ni diálogo de portal;
  - la captura es directa: la misma API entrega los frames.
- **Riesgos:**
  - Es una API **privada** de Mutter (la usa gnome-remote-desktop), no estable formalmente, así que hay que fijar una matriz de versiones probadas.
  - Cambiar la resolución implica renegociar el formato del *stream*.
- **Entrada:** ver §3.3.

**KWin (KDE Plasma 6)**
- **API:** `zkde_screencast_unstable_v1.stream_virtual_output(name, size, scale)`. El cliente necesita autorización (el `.desktop` declara `X-KDE-Wayland-Interfaces`). Alternativa: el portal ScreenCast con `types = VIRTUAL`.
- **Pendiente de la PoC-7:**
  - la versión mínima de Plasma;
  - si el portal KDE permite `VIRTUAL` sin interacción la segunda vez, usando `restore_token`.

**wlroots (Sway, Hyprland, river…)**
- Se crea una salida *headless* por IPC del compositor y luego se captura con el portal wlr o con `ext-image-copy-capture`.
- Cada compositor tiene su propia IPC, así que se soportan **Sway y Hyprland** de forma explícita y el resto queda como "mejor esfuerzo".

**EVDI (el *fallback* universal, incluido X11)**
- **Cómo funciona:** el módulo `evdi` (GPL-2.0, DKMS) crea dispositivos DRM. Desde espacio de usuario, `libevdi` (LGPL-2.1, **enlace dinámico**) "conecta" un monitor con un EDID generado por nosotros y recibe los *buffers* actualizados.
- **Ventajas:**
  - Es compositor-agnóstico: funciona en X11 y en Wayland (Mutter y KWin soportan dispositivos EVDI como DisplayLink).
  - Es el único backend que da un monitor "de verdad" en X11 sin trucos.
- **Inconvenientes:**
  - **Módulo DKMS** que debe compilarse para cada kernel.
  - **Secure Boot** obliga a firmar el módulo (MOK).
  - La captura con `evdi_grab_pixels` copia a CPU (≈ 8 MB por frame a 1080p).
  - En Wayland, el rendimiento depende de la copia entre GPUs del compositor.
- **Estrategia:** no se incluye el módulo en el paquete; se recomienda el de la distribución (`evdi-dkms` en Ubuntu, Arch y Fedora vía RPM Fusion). El helper privilegiado solo hace `modprobe evdi initial_device_count=N` y se instala una regla udev para el grupo `video`.

**X11 sin EVDI**
- Para el driver `intel` con `VirtualHeads`, o para conectores desconectados forzados con `xrandr --addmode`, el diagnóstico guía al usuario.
- **No se escriben archivos de sistema** como `/etc/X11/xorg.conf.d` ni `/sys/.../status` con polkit sin contraseña (diferencia deliberada con TwinDesk).

### Selección automática de backend
1. Detectar `XDG_SESSION_TYPE` y `XDG_CURRENT_DESKTOP`, y sondear los servicios D-Bus o los globales Wayland disponibles.
2. Por orden de prioridad, elegir el primer backend cuyo `capabilities()` sea viable.
3. Mostrar en la interfaz **qué backend se usa y por qué**. Si ninguno es viable, mostrar un diagnóstico con los pasos concretos (por ejemplo, "instala `evdi-dkms`").

---

## 3.3 Entrada asociada al monitor virtual

En modo extender, el ratón y el teclado son los de la fuente, así que **no hace falta inyectar nada**. Inyectar solo es necesario si el receptor aporta entrada (pantalla táctil o KVM inverso).

| SO | Mecanismo preferido | *Fallback* |
|---|---|---|
| Windows | `SendInput` (teclado y ratón); `InjectSyntheticPointerInput` (táctil y pluma) | — |
| Linux Wayland | **libei** a través del portal **RemoteDesktop** (`ConnectToEIS`): los eventos absolutos se asocian al *stream* del monitor virtual, la vía oficial en GNOME 46+ y KDE 6.1+ | `uinput` (requiere grupo `input`; el mapeo absoluto en multimonitor es frágil, como observamos en Weylus) |
| Linux X11 | XTest | `uinput` |

---

## 3.4 Captura en modo duplicar (sin monitor virtual)

| SO | Primario | *Fallback* |
|---|---|---|
| Windows | DXGI Desktop Duplication | Windows.Graphics.Capture |
| Linux Wayland | Portal ScreenCast con PipeWire (DMA-BUF) y `restore_token` **persistido**, para no mostrar el diálogo en cada sesión (Weylus nunca lo reutiliza) | — |
| Linux X11 | XShm + XDamage | KMS/DRM (requiere `CAP_SYS_ADMIN`; no se usa por defecto) |
