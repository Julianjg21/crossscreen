# 1. Análisis comparativo de los repositorios de referencia

**Método.** Se clonó cada repositorio (HEAD a fecha 2026-11-08) y se auditó su **código fuente**, no solo el README. Cada afirmación lleva la ruta del archivo que la respalda. Una funcionalidad se marca como *Implementada* solo si se encontró el código que la ejecuta.

| Repositorio | Último commit revisado |
|---|---|
| InukaAsith/TwinDesk | 2026-11-05, v2.5.3 |
| pavlobu/deskreen | 2026-07-08, v3.2.16 |
| H-M-H/Weylus | 2026-02-16, v0.11.4 |
| LizardByte/Sunshine | 2026-11-07 |
| ScreenExtend/ScreenExtend | 2026-10-28, v0.7.0 |

---

## 1.1 Resumen ejecutivo

| | TwinDesk | Deskreen | Weylus | Sunshine | ScreenExtend |
|---|---|---|---|---|---|
| **Licencia** | GPL-3.0 | AGPL-3.0 | AGPL-3.0-or-later | GPL-3.0 | AGPL-3.0 |
| **Lenguaje principal** | JS (Electron) + C#/C/Swift | TS (Electron) | Rust + C | C++ | Rust (Tauri) + TS |
| **Receptor** | App nativa (Electron) | Navegador | Navegador | Moonlight (externo) | Navegador |
| **Monitor virtual Windows** | ✅ Driver de terceros (Virtual-Display-Driver) | ❌ | ❌ | ❌ (solo reconfigura monitores existentes) | ✅ Fork de virtual-display-rs |
| **Monitor virtual Linux** | ⚠️ Solo GNOME/Mutter (Wayland); en X11 fuerza un conector libre | ❌ | ❌ (manual, solo en el README) | ❌ | ❌ Stub |
| **Captura** | Chromium (`desktopCapturer`) | Chromium | XShm, PipeWire (CPU), DXGI DD (CPU) | DXGI DD, WGC, KMS, wlroots, portal, KWin, NvFBC | DXGI DD, WGC, ScreenCaptureKit |
| **Codificación** | WebRTC de Chromium (H.264, MF en Windows) | WebRTC de Chromium (normalmente VP8) | FFmpeg (VAAPI/NVENC/MF/x264) | NVENC directo, FFmpeg (QSV/AMF/VAAPI/Vulkan/MF/x264) | NVENC/QSV/AMF directos, x264 |
| **Transporte** | WebRTC + WSS + UDP propio para entrada | WebRTC + socket.io por HTTP | WebSocket por HTTP (fMP4) | RTP/UDP + ENet + RTSP | WebRTC (webrtc-rs) + WHEP |
| **Cifrado real** | ✅ TLS fijado + SAS + DTLS-SRTP + AES-GCM | ⚠️ Solo DTLS-SRTP (señalización en claro, vulnerable a MITM) | ❌ Ninguno | ⚠️ Control siempre; vídeo **apagado en LAN por defecto** | ⚠️ DTLS-SRTP; señalización HTTP o autofirmada sin fijar |
| **Emparejamiento** | Commit/reveal + SAS de 6 dígitos | ID de sala de 6 dígitos + aprobación manual | Código en claro en la URL | PIN de 4 dígitos + AES-128-ECB (atacable offline) | OTP con límite de intentos + tokens de dispositivo |
| **mDNS** | ❌ (balizas UDP propias) | ❌ | ❌ | ✅ (`_nvstream._tcp`) | ❌ |
| **Entrada remota** | ✅ | ❌ | ✅ (pluma y táctil excelentes) | ✅ | ✅ Windows y macOS / ❌ Linux |
| **Portapapeles** | ✅ | ❌ | ❌ | ❌ | ⚠️ Parcial |
| **Audio** | ✅ Opus | ❌ | ❌ | ✅ Opus | ✅ Opus (Windows y macOS) |

**Conclusiones clave**

1. **Ninguno cubre el caso completo**: un monitor virtual real en ambos SO, con un receptor nativo en ambos SO.
   - TwinDesk es el que más se acerca, pero en Linux depende de GNOME o de trucos con conectores físicos.
   - Además, funciona sobre Electron, que limita la latencia y el control del códec.
2. **Las cinco licencias son copyleft fuerte.** Para un proyecto `MIT OR Apache-2.0` solo se pueden reutilizar **ideas** y **dependencias upstream permisivas**:
   - Virtual-Display-Driver (MIT)
   - virtual-display-rs (MIT)
   - windows-capture (MIT)
   - ViGEm (MIT/BSD)
3. **En Linux nadie resuelve el monitor virtual de forma genérica.** Es el mayor riesgo técnico del proyecto (ver [03-monitores-virtuales.md](03-monitores-virtuales.md)).
4. **La seguridad es mediocre en cuatro de los cinco.** Solo TwinDesk autentica el intercambio de claves. CrossScreen debe superar a todos en este punto, no imitarlos.
5. **Sunshine es la mejor referencia de pipeline de baja latencia**: captura y codificación *zero-copy*, FEC, *pacing*, invalidación de referencias. Aun así, no hace monitores virtuales ni tiene cliente.

---

## 1.2 TwinDesk

**Qué es.** Una suite de "continuidad" (KVM, portapapeles, archivos, Android, audio, cámara virtual, etc.) para Windows, Linux, macOS y Android. El monitor extendido es una funcionalidad más entre decenas.

- **Tamaño:** unos 190 000 líneas.
  - Electron 44 por SO.
  - *Helpers* nativos en C# (Windows), C (Linux) y Swift (macOS).
  - Un crate Rust pequeño, `native/tdfast`.
- **Organización:** árboles casi duplicados por SO (`windows/`, `linux/`, `mac/`, con variantes `mesh/`).

### Arquitectura
- **Roles:** el anfitrión expone un servidor HTTPS/WSS en TCP 47800 (`windows/server.js`) y el invitado se conecta con `windows/link.js`.
- **Pantalla:** el rol de pantalla es independiente del de anfitrión e invitado.
  - El lado que comparte captura y envía la oferta WebRTC (`pcscreen.js`).
  - El visor responde en modo *recvonly* (`monitor.js`).
  - No se captura nada si no hay nadie mirando.
- **Windows ⇄ Linux:** funciona en ambos sentidos. El código Linux es una copia del de Windows (`linux/pcscreen.js`).

### Monitor virtual
**Windows: real.**
- Descarga e instala **VirtualDrivers/Virtual-Display-Driver 25.7.23** (IddCx, MIT, firmado por SignPath Foundation). No lo incluye en el paquete.
- El instalador (`windows/scripts/vdd-install.ps1`) hace estas comprobaciones e instalación:
  1. verifica el SHA-256;
  2. comprueba que la firma Authenticode de `mttvdd.cat` esté emitida a `CN=SignPath Foundation`;
  3. lo instala con elevación.
- Registra dos tareas programadas como SYSTEM para activar y desactivar el dispositivo PnP sin UAC.
- **Modos:** las resoluciones se escriben en `C:\VirtualDisplayDriver\vdd_settings.xml` (con regex).
- **Extender:** fuerza el modo extendido mediante su helper (`displayExtend`, id `MTT1337`).
- **Windows on ARM:** falla (error `0x800B0109`) porque el driver no tiene firma de Microsoft.

**Linux:** tres caminos, todos en `linux/vdd.js`.
- **GNOME Wayland:** `org.gnome.Mutter.ScreenCast.RecordVirtual`, a través de un script Python/GStreamer embebido. Es un monitor real del compositor, pero **solo funciona en GNOME**.
- **X11:** toma un conector DRM **desconectado** y lo fuerza a "on" escribiendo en `/sys/class/drm/cardN-X/status`.
  - Lo hace con pkexec y una regla polkit `allow_active=yes`, es decir, como root sin contraseña.
  - Necesita una salida física libre.
- **X11 con NVIDIA propietario:** escribe un `xorg.conf.d` con EDID falso y exige volver a iniciar sesión.
- **No hay soporte** para EVDI, KDE ni wlroots.

### Pipeline
- **Captura:** `desktopCapturer` y `getUserMedia` de Chromium. En Wayland usa el portal y PipeWire mediante `WebRTCPipeWireCapturer`, y el usuario tiene que elegir el monitor virtual en el diálogo cada vez.
- **Códec:** H.264 por defecto (H.265 opcional) a través del stack WebRTC de Chromium (`shared/screencodec.js`).
  - En Windows usa Media Foundation por hardware, con caída a OpenH264 si el codificador se bloquea (`StallWatch` en `windows/encoder.js`).
  - En Linux no se encontraron *flags* de VAAPI, así que la aceleración por hardware es probablemente inexistente.
- **Trucos de latencia:**
  - *field trials* `WebRTC-ForceSendPlayoutDelay/min_ms:0,max_ms:0`;
  - FlexFEC-03;
  - `jitterBufferTarget=0` y `playoutDelayHint=0`;
  - `degradationPreference='maintain-framerate'`.

### Seguridad: la mejor de las cinco
- **Certificado:** autofirmado y **fijado por huella SHA-256** (`windows/link.js:214-220`).
- **Emparejamiento:** commit/reveal con un **SAS de 6 dígitos**, sha256(label‖fp‖nonces) mod 10⁶ (`shared/protocol.js:474-488`).
- **Protección de tokens:** comparación en tiempo constante y limitador de intentos.
- **Canal de entrada:** UDP propio "tdv2" con AES-256-GCM, claves HKDF por dirección, nonce = número de secuencia y ventana anti-repetición de 64 paquetes (`native/tdfast/src/wire.rs`).
- **Descubrimiento:** balizas UDP propias en el puerto 47801, **no mDNS**.

### Anunciado frente a implementado
| Afirmación | Veredicto |
|---|---|
| Pantalla virtual real en Windows | ✅ Implementado (driver de terceros) |
| Extender en Linux (X11 y Wayland) | ⚠️ Parcial: en Wayland solo GNOME; en X11 requiere un conector libre o NVIDIA |
| Codificación por hardware | ✅ Windows / ❓ Linux, probablemente software |
| Cifrado de extremo a extremo | ✅ Implementado |
| Descubrimiento de dispositivos | ✅ Implementado, pero no es mDNS |

### Lo aprovechable (solo ideas)
- El flujo de instalación del driver: release fijada, hash, sujeto Authenticode y activación por PnP.
- El uso de `RecordVirtual` de Mutter.
- El diseño de emparejamiento SAS y del canal de entrada AES-GCM con ventana anti-repetición.
- El detector de bloqueo del codificador.

### Banderas rojas
- Acciones root sin contraseña en Linux.
- Tareas que se ejecutan como SYSTEM.
- Edición de XML con regex.
- Código duplicado por SO.
- Dependencia de *field trials* de Chromium, que pueden romperse entre versiones.

---

## 1.3 Deskreen

**Qué es.** Una app Electron que transmite una pantalla o ventana existente a **un navegador** de la LAN mediante WebRTC.
- Unas 13 400 líneas de TypeScript.
- El servidor de señalización Koa + socket.io está portado de darkwire.io (MIT).

### Hallazgos
- **Monitor virtual: no existe.** No hay driver, IddCx, EVDI ni xrandr en el código. Cualquier uso como "pantalla extendida" depende de un *dummy plug* HDMI o de un driver externo. La wiki es un *placeholder* de dos líneas.
- **Captura:** `desktopCapturer` de Electron. En Wayland usa el portal (`main/utils/isLinuxWaylandSession.ts`, `ipcMainHandlers.ts:63-90`).
- **Códec:** lo decide Chromium. No hay `setCodecPreferences` ni manipulación del SDP, así que suele acabar en VP8 por software. La única afinación es `webrtc-max-cpu-consumption-percentage=100`.
- **Transporte:** simple-peer con `iceServers: []`, así que solo funciona en LAN. La señalización va por **HTTP en claro**.
- **Cifrado: el nombre engaña.** Las funciones `prepare/process` de `utils/message.ts` ("encrypted message") son la **función identidad**. Se eliminó el cifrado de darkwire.
  - La única protección es DTLS-SRTP.
  - Como las huellas viajan por HTTP sin autenticar, un atacante activo en la LAN puede hacer MITM.
- **Emparejamiento:** el ID de sala son 6 dígitos (`crypto.randomBytes(3)`) y el anfitrión aprueba manualmente. El "propietario" se determina porque la IP sea `127.0.0.1`.
- **Sin entrada remota, sin audio, sin portapapeles y sin mDNS** (se conecta con código QR o URL).
- **Ventana auxiliar insegura:** usa `nodeIntegration: true` y `sandbox: false`.

### Anunciado frente a implementado
| Afirmación | Veredicto |
|---|---|
| "Cualquier dispositivo con navegador como segunda pantalla" | ⚠️ Solo duplica una pantalla o ventana existente |
| Pantalla virtual o extendida | ❌ No existe |
| Cifrado E2E (crédito a tweetnacl) | ❌ tweetnacl ni siquiera está en `package.json` |
| Calidad automática | ⚠️ Rudimentaria: compara píxeles con pixelmatch y solo baja la resolución |

### Lo aprovechable (solo ideas)
- La experiencia de emparejamiento: QR, diálogo de aprobación con datos del dispositivo y bloqueo de la sala tras conectar.
- La heurística para elegir la interfaz LAN filtrando las NIC virtuales.

**Es un ejemplo de lo que hay que evitar en seguridad.**

---

## 1.4 Weylus

**Qué es.** Convierte una tablet o teléfono, **a través de su navegador**, en tableta gráfica o pantalla táctil del PC.
- Rust (unas 6 500 líneas) más C (unas 2 100 líneas, FFI con FFmpeg y uinput) más un cliente TS.
- Mantenimiento lento: el último cambio solo toca CI. Bus factor ≈ 1.

### Hallazgos
- **Monitor virtual: no existe.** Las "salidas virtuales" de Intel y nouveau son **configuración manual con xrandr documentada en el README**. El código solo *consulta* monitores (`lib/linux/xhelper.c`).
  - En Wayland pasa al portal la máscara de tipos de fuente, que incluye `VIRTUAL=4`. Si el backend del portal lo admite, *podría* crear un monitor virtual, pero es incidental y no está verificado.
- **Captura: todo copia por CPU.**
  - X11: XShm.
  - Wayland: PipeWire mediante un `appsink` de GStreamer, sin DMA-BUF. El `restore_token` se imprime con `dbg!` y nunca se reutiliza (hay un TODO).
  - Windows: `captrs`, que usa DXGI con *readback*.
  - macOS: `CGDisplay::image()` en cada frame.
- **Codificación:** solo H.264, en `lib/encode_video.c`.
  - Los backends hardware son opcionales y están apagados por defecto: VAAPI, MF (`scenario=display_remoting`), NVENC (`p1/ull/zerolatency`) y VideoToolbox. El fallback es x264 `ultrafast/zerolatency`.
  - Usa `gop_size=12` y ningún frame B.
- **Contenedor:** fMP4 con un fragmento por frame, reproducido con **MSE** en el navegador. Esto añade *buffering*, y para resincronizar salta a `seekable.end`.
- **Transporte:** WebSocket sobre HTTP (hyper + fastwebsockets), mensajes de control en JSON y fMP4 en frames binarios.
- **Seguridad: inexistente.**
  - **No hay TLS**; el README recomienda un proxy externo (`weylus_tls.sh` con hitch).
  - El código de acceso va **en la query string**, en claro, se compara con `==` (no en tiempo constante) y no tiene límite de intentos.
  - Escucha en `0.0.0.0`.
- **Entrada: el punto fuerte.**
  - uinput con dispositivos separados de teclado, ratón, pluma y táctil (`lib/linux/uinput.c`): pluma con `ABS_PRESSURE` y `ABS_TILT_X/Y` (±90), táctil multitáctil MT-B de 5 *slots* y `INPUT_PROP_DIRECT`.
  - En Windows, pluma y táctil con `InjectSyntheticPointerInput` (`autopilot_device_win.rs`).
  - Los métodos `Notify*` del portal RemoteDesktop están enlazados pero **nunca se llaman**.
- **FFmpeg con licencia GPL:** el binario incluido se compila con `--enable-gpl --enable-libx264`.

### Lo aprovechable (solo ideas)
- El flujo del portal RemoteDesktop + ScreenCast (reimplementar con `ashpd`).
- El diseño de dispositivos uinput para pluma y táctil (reimplementar con el crate `evdev`).
- Los parámetros de baja latencia del codificador como referencia.

**Lo que hay que evitar:**
- fMP4 + MSE;
- FFmpeg con licencia GPL;
- un TLS añadido a posteriori.

---

## 1.5 Sunshine

**Qué es.** Un servidor de *streaming* compatible con GameStream; el cliente es Moonlight.
- Unas 73 000 líneas de C++. Muy activo: commit del 2026-11-07.
- Los submódulos incluyen `moonlight-common-c` (GPL-3.0), libdisplaydevice, libvirtualhid, ViGEmClient y Simple-Web-Server.

### Hallazgos
**Monitor virtual: no existe.**
- No aparece SudoVDA, IddCx, VDD ni EVDI.
- `src/display_device.cpp` (envoltorio de libdisplaydevice, solo Windows y macOS) **reconfigura monitores existentes**: los activa, los hace primarios, cambia la resolución y el HDR, y lo revierte al terminar.

**Captura: la más completa.**
- **Windows:** DXGI Desktop Duplication, verificado (`display_base.cpp`, `DuplicateOutput1`), con caída a WGC (`display_wgc.cpp`). El cursor se compone con un *shader* D3D11.
- **Linux:** seis backends.
  - KMS/DRM con DMA-BUF (`kmsgrab.cpp`, necesita `CAP_SYS_ADMIN`);
  - NvFBC;
  - wlr-screencopy;
  - X11;
  - portal con PipeWire y `restore_token`;
  - KWin `zkde_screencast_unstable_v1`.

**Codificación: la referencia a seguir.**
- **Abstracción:** los `encoder_t` llevan *flags* de capacidad y hay un sondeo al arrancar (`src/video.cpp`).
- **NVENC directo en Windows** (`src/nvenc/*`) con:
  - `ULTRA_LOW_LATENCY`, CBR y sin frames B;
  - `zeroReorderDelay` y VBV de un solo frame;
  - *intra refresh* opcional;
  - **invalidación real de frames de referencia**.
- **Resto de backends:** van por FFmpeg (QSV, AMF, MF, VAAPI, Vulkan Video, x264/x265/SVT-AV1). Solo NVENC directo invalida referencias; los demás piden un IDR.
- **Zero-copy:** textura D3D11 hacia el codificador en Windows; DMA-BUF hacia VAAPI, CUDA o Vulkan en Linux.

**Transporte:**
- RTP sobre UDP para el vídeo, con **FEC Reed-Solomon** (nanors) en porcentaje configurable.
- *Pacing* de paquetes dentro de cada frame y envío por lotes (GSO / `sendmmsg`).
- Marcado DSCP.
- ENet para control y entrada; RTSP para negociar la sesión.

**Cifrado:**
- Control siempre con AES-128-GCM.
- **Vídeo opcional: `lan_encryption_mode = NEVER` por defecto** (`config.cpp:844`).
- Audio en AES-128-CBC.

**Emparejamiento débil:**
- **PIN de 4 dígitos**: la clave es SHA-256(salt‖PIN) con **AES-128-ECB** (`crypto.cpp:226-325`).
- Quien observe el emparejamiento puede romper el PIN por fuerza bruta offline.
- Tras emparejar, usa TLS mutuo con certificados autofirmados.

**mDNS:** nativo de cada plataforma (Avahi con `dlopen`, `DnsServiceRegister` en Windows).

**Entrada:** libvirtualhid. Los dispositivos HID reales en Windows **requieren licencia de pago**; sin ella cae a SendInput. En Linux usa uinput.

**Portapapeles:** no implementado.

### Anunciado frente a implementado
| Afirmación | Veredicto |
|---|---|
| Captura multi-backend | ✅ Implementado |
| NVENC, AMF, QSV, VAAPI, Vulkan | ✅ Implementado |
| Invalidación de referencias | ⚠️ Solo NVENC directo |
| Cifrado | ⚠️ Vídeo apagado en LAN por defecto |
| Pantallas virtuales | ❌ No existe |
| Portapapeles | ❌ No existe |

### Lo aprovechable (solo conceptos, reimplementados desde cero)
- FEC por frame.
- *Pacing* y envío por lotes con GSO.
- Configuración del codificador (CBR, VBV de un frame, sin frames B).
- IDR bajo demanda e invalidación de referencias guiadas por informes de pérdida.
- Cadenas de *fallback* de captura y sondeo de codificadores.

**No copiar:**
- ningún código, ni de `src/` ni de moonlight-common-c;
- **el diseño del emparejamiento** (es débil);
- la política de vídeo sin cifrar en LAN.

---

## 1.6 ScreenExtend

**Qué es.** Una app **Tauri 2 + React**, el stack más parecido al de CrossScreen.
- **Solo hace de fuente** (Windows y macOS). El receptor es una página web estática servida por la fuente.
- Unas 104 000 líneas de Rust en un único crate y unas 12 900 líneas de TS.

### Hallazgos
- **Monitor virtual en Windows: real.**
  - Usa IddCx 1.2 con un **fork privado y recompilado de MolotovCherry/virtual-display-rs** (MIT upstream), que controla por la tubería con nombre `\\.\pipe\virtualdisplaydriver` (`windows_utils/virtual_display.rs`).
  - El fork **no publica su código fuente**: la DLL se compiló en la máquina del autor.
  - El instalador mete un certificado autofirmado en `Root` y `TrustedPublisher` del sistema.
- **Monitor virtual en macOS: real.** Usa la API privada `CGVirtualDisplay`.
- **Linux:** `linux_utils/virtual_display.rs` devuelve `Err("virtual displays are not yet implemented on Linux")`. Captura, entrada y audio en Linux también son *stubs*.
- **Captura en Windows:**
  - fork de windows-capture (WGC, MIT upstream);
  - DXGI DD propio con cursor superpuesto;
  - todo permanece en GPU (D3D11).
- **Codificación:** FFI escrito a mano y cargado con `libloading`, sin FFmpeg.
  - NVENC (D3D11 directo, *intra refresh*): real.
  - QSV: real.
  - AMF: el propio archivo dice *"nothing here has ever encoded a frame"*.
  - x264: DLL GPL incluida.
  - VideoToolbox: real.
- **Transporte:** WebRTC con webrtc-rs 0.17, señalización WHEP sobre axum.
  - Tiene su propio *packetizer* RFC 6184 porque el `H264Payloader` de webrtc-rs descarta SPS/PPS.
  - Tiene su propio *pacer* porque webrtc-rs no trae uno.
  - Tiene control de congestión TWCC del lado emisor.
- **Seguridad:**
  - OTP comparado en tiempo constante (`subtle`), con bloqueo tras 5 intentos y pausa global, y tokens de confianza por dispositivo.
  - La señalización va por HTTP, o por HTTPS autofirmado **sin fijar el certificado**.
- **Receptor:** WebCodecs `VideoDecoder` en un *worker* con `RTCRtpScriptTransform`, dibujando en un OffscreenCanvas. **No hay decodificador nativo.**
- **Sin mDNS.**
- **Calidad del código:**
  - 728 tests y CI con clippy `-D warnings` y una puerta de rendimiento del codificador.
  - **`SQUASH.md` declara** que todo lo posterior al 2026-09-06 lo generó un agente de IA en una sola ejecución autónoma (~25 h, 562 commits, +146 000 líneas) y llama al árbol actual *"fast, buggy"*.

### Anunciado frente a implementado
| Afirmación | Veredicto |
|---|---|
| Pantalla virtual en Windows y macOS | ✅ Implementado |
| Fuente Linux | ❌ Stub |
| NVENC, QSV, VideoToolbox | ✅ Implementado |
| AMF | ⚠️ Sin probar |
| Cifrado E2E | ⚠️ Parcial: sin fijación de certificado |
| mDNS | ❌ No existe |
| Receptor nativo | ❌ No existe (navegador) |

### Lo aprovechable
- Usar **virtual-display-rs upstream (MIT)** directamente, no el fork.
- Las lecciones documentadas sobre IddCx 1.2:
  - la lista de modos es fija por monitor, así que cambiar de modo obliga a recrearlo;
  - no se puede elegir el adaptador de renderizado.
- El patrón de cargar los SDK de los fabricantes con `libloading` por niveles de capacidad.
- Las advertencias sobre webrtc-rs.

---

## 1.7 Matriz de reutilización para CrossScreen

| Componente | Origen permitido | Licencia | Uso |
|---|---|---|---|
| Driver IddCx ya firmado | VirtualDrivers/Virtual-Display-Driver | MIT (✅ verificado el 2026-11-08) | Dependencia externa instalada en tiempo de ejecución (fases 1-3) |
| Driver IddCx en Rust | MolotovCherry/virtual-display-rs | MIT (✅ verificado el 2026-11-08) | Referencia o base para un driver propio (fase 8) |
| Captura WGC | NiiightmareXD/windows-capture | MIT | Crate de Rust |
| Portal XDG | `ashpd` | MIT | Crate de Rust |
| PipeWire | `pipewire` (pipewire-rs) | MIT | Crate de Rust |
| EVDI | DisplayLink/evdi | Módulo del kernel GPL-2.0, `libevdi` LGPL-2.1 | Enlace dinámico con `libevdi`; el módulo lo instala el usuario o la distribución |
| Gamepad virtual en Windows | ViGEmClient | MIT | Opcional, en el futuro |
| **Código de TwinDesk, Deskreen, Weylus, Sunshine y ScreenExtend** | — | GPL/AGPL | **Prohibido.** Solo como estudio de diseño |

**Verificación del 2026-11-08** (archivos LICENSE upstream):
- Virtual-Display-Driver: MIT.
- virtual-display-rs: MIT.
- windows-capture: MIT.
- EVDI: la raíz es MIT, pero `library/` es **LGPL-2.1** y `module/` es **GPL-2.0**, lo que coincide con la tabla.

`cargo-deny` vigila en CI las licencias de los crates.
