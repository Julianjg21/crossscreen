# Estado del proyecto (memoria compartida entre agentes)

> **Cómo usar este archivo:**
> - Léelo al empezar.
> - Al terminar tu sesión, actualiza "Trabajo en curso", "Próximos pasos" y añade una línea a la **Bitácora**: fecha, quién, qué se hizo y qué quedó a medias.
> - Las reglas permanentes están en [`AGENTS.md`](../AGENTS.md).

**Última actualización:** 2026-11-08 (coordinador). **Receptor Linux (Windows → Linux)** implementado y validado en Ubuntu real y previamente en WSL2; paquete `linux-x64.tar.gz` en la release. El panel visual Linux ya está configurado para generar AppImage y `.deb`, incluyendo el agente Linux como recurso; el emisor Linux experimental conecta Portal/PipeWire, OpenH264 y QUIC para duplicación, pendiente de validación real.

## Resumen en una frase
La cadena completa de Windows funciona de punta a punta (monitor virtual → captura → H.264 por hardware → QUIC cifrado con reconexión automática → decodificación → presentación con cursor + entrada remota de ratón y teclado), **validada y afinada sobre Wi-Fi real entre dos equipos (60 s a `1366×768@60`: pérdidas reducidas de 31 a 2 frames, peticiones `RequestKeyframe` de 104 a 1, cursor de 638 a 3 040 posiciones y `Captura→enviado` p50 18,1 / p95 25,6 / máx. 53,2 ms)** e integrada en `crossscreen-agent` y en el panel de escritorio Tauri (`Transmitir pantalla`).

## Fases (ver `docs/07-roadmap.md`)
| Fase | Estado |
|---|---|
| F0 Fundamentos | ✅ En lo esencial (CI y release funcionando en GitHub) |
| F1 PoC (Windows) | ✅ PoC-1, 2, 3 y 5; PoC-4 en loopback + pruebas reales Wi-Fi |
| F2 MVP Windows → Windows | 🟡 Funciona en Wi-Fi real (`PC-A → PC-B`), integrado en agente + panel Tauri y con reconexión QUIC automática; falta prueba por cable y estabilidad de 2 h |
| F3 Extender en Windows | ✅ VDD + CCD funcionando, integrado en agente y con ventana de gracia de 15 s ante cortes de red |
| F4 Linux | 🟡 **Receptor** hecho (OpenH264 de Cisco + wgpu, X11/Wayland), validado en Ubuntu real y WSL2; emisor experimental en `duplicate`; faltan VAAPI, portapapeles y `extend` |
| F5 Robustez y Wi-Fi | 🟡 FEC adaptativo + entrelazado, `DecodeThrough`, `KeyframeRequester` calmado y reconexión con ventana de gracia implementados |
| F6 Entrada remota / KVM | 🟡 Protocolo `ControlMessage::Input`, crate `cs-input` (`SendInput` sobre escritorio virtual + liberación automática al desconectar) y captura en `crossscreen-viewer` integrados en Windows |

## Entorno de prueba real
- **PC-A (fuente, se extiende):** portátil del dueño `DESKTOP-6K2V76S`.
  - Intel Iris Xe, panel 1920×1080 al 125 %.
  - Wi-Fi `192.168.1.134`.
  - Driver VDD 25.7.23 **instalado**.
  - Huella `8075-2f40-0208-a313`.
- **PC-B (receptor, hace de pantalla):** `DESKTOP-93VFE2P`.
  - Panel 1366×768.
  - IP `192.168.1.127`.
  - Huella `ff81-e0ec-06c2-3137`.
  - Usa el `.zip` de la release, en `C:\Users\Julian\Downloads\crossscreen-0.0.1-windows-x64`.
- **A y B ya están emparejados**, cada uno en su carpeta de datos por defecto.
- **Cómo lanzar:**
  - En B: `.\crossscreen-viewer.exe --listen 0.0.0.0:47810 --fullscreen`
  - En A: `crossscreen stream 192.168.1.127:47810 --peer ff81 --seconds 60`
  - Guía completa: `docs/10-prueba-dos-equipos.md`.
- **Identidades de prueba para loopback en A:** `…/scratchpad/rx/A` (receptor, huella `8492…`) y `rx/B` (emisor, huella `772a…`), ya emparejadas entre sí. Viven en el scratchpad de la sesión de Claude Code y pueden desaparecer; si no existen, crea otras dos con `--data-dir` y empareja con `pair-listen` / `pair`.

## Resultados clave medidos
- **Mismo equipo (loopback):** captura → `Present()` p50 29 / p95 32 ms, 0 pérdidas.
- **Codificación MF en la Iris Xe:** 1080p60 p95 ≈ 11 ms (objetivo ≤ 6 ms: **no se cumple**); 1366×768 p50 6,7 / p95 9,4 ms.
- **Primera prueba real (Wi-Fi, 60 s, 1366×768):**
  - Funcionó; el dueño pudo manejar la segunda pantalla.
  - **Mucho lag, saltos bruscos y congelaciones.**
  - 31 frames perdidos (pérdidas en **ráfaga**: 0 datagramas tardíos, solo 18 shards recuperados con FEC 10 %).
  - 104 peticiones de IDR y 90 IDR forzados.
  - Recuperación de 0,4 a **6,4 s**; 213 frames descartados esperando IDR.
  - RTT inicial 943 ms y *jitter* 38 ms (probable ahorro de energía del Wi-Fi).
  - **Diagnóstico:** círculo vicioso. Una pérdida congela hasta el siguiente IDR; los IDR (~80 KB) también se pierden en ráfagas; las peticiones repetidas saturan el Wi-Fi.
- **Resiliencia ante pérdidas en ráfaga (`v0.0.1-alpha.2`, 15 s / 900 frames a 60 fps bajo Gilbert-Elliott `p=0,015, r=0,25, h=0,8`, ~4,75 % de pérdida en ~500 ráfagas):**
  - Frames perdidos post-FEC: de **133–144** (15–16 %) a **14** (**1,6 %, -90 %**).
  - Shards recuperados por FEC: de **424–472** a **1 082** (`AdaptiveFec` sube de `0,15 → 0,23 → 0,34 → 0,50` en 3 ajustes).
  - Peticiones `RequestKeyframe`: de **94–144** a **12** (**-87 % a -92 %**, exactamente 1,00 por episodio, recuperadas en **18,1 ms p50 / 22,1 ms máx.**).
  - Congelaciones (> 200 ms): de **3 congelaciones (867 ms total, máx. 367 ms)** y 217 frames descartados a **0 congelaciones (0 ms)** y **853 frames presentados (56,8 fps)** frente a 545 (36,3 fps).
- **Verificación de `CODECAPI_AVEncVideoGradualIntraRefresh` en el MFT de Intel:** acepta `1` y `0x00010001` en `SetValue`/`GetValue`, pero las 120/120 unidades de acceso generadas son **100 % idénticas byte a byte** a `None (0)` (no-op total en el wrapper MFT de Intel).
- **Validación y afinado en Wi-Fi real (`PC-A → PC-B`, 60 s, `1366×768@60`):**
  - **Frames perdidos post-FEC:** de **31** (`v0.0.1-alpha.1`) y **19** (`v0.0.1-alpha.2`) a **2** (**0,11 %** de 1 781 frames, **0 frames perdidos sin ningún datagrama**).
  - **Peticiones `RequestKeyframe`:** de **104** a **1** en 60 s (**1,00 por episodio**, 0 por espera vencida, 0 frames descartados esperando IDR).
  - **Latencia y estabilidad de envío:** corregido el cálculo de `paced = total - avail` y `set_rate_at` en `Pacer::schedule` (antes un frame apenas por encima de `burst_bytes` con *bucket* vacío se programaba a ~14 s vista), escalado el objetivo CBR por defecto según área (`8 094 kbps` en `1366×768`, P-frames de `40,3 → 16,4 KiB`, IDR de `103,0 → 31,3 KiB`) y acotado el ritmo máximo de `Spread`; `Captura→enviado (QPC)` queda en **p50 18,13 / p95 25,62 / máx. 53,18 ms** y *jitter* en **10,7 ms** (-73 %).
  - **Fluidez del cursor:** sondeo Win32 (`GetPhysicalCursorPos` + `GetCursorInfo`) a **125 Hz** independiente del codificador, retorno inmediato en `DxgiCapture::next_frame` ante eventos de puntero y deduplicación de `CursorShape`, subiendo de **638** posiciones (`10,6 Hz`) a **3 040** (`50,7 Hz`) y **846–1 122** repintados solo de cursor.

## Trabajo en curso (2026-11-08)
**Receptor Linux validado por el dueño en Ubuntu real** (PC-A Windows → Ubuntu). Guía: `docs/11-receptor-linux.md` (va en el paquete como `RECEPTOR-LINUX.md`). No se han añadido métricas nuevas porque la prueba confirmada no incluyó un resumen numérico.

Cómo está hecho:
- `cs-decode` en no-Windows: crate `openh264` (feature `libloading`) que carga **el binario oficial de Cisco** `libopenh264-2.6.0-linux64.8.so` (lo descarga y verifica `scripts/fetch-openh264.sh`; no se redistribuye por la licencia de patentes). Busca en `CROSSSCREEN_OPENH264`, junto al ejecutable y en `~/.local/share/crossscreen/`. Frames I420 en CPU.
- `cs-render` en no-Windows: `wgpu` 25 (Vulkan/GL), tres texturas R8 → RGB BT.709 en shader, cursor superpuesto. `capture_rgba` no implementado.
- `apps/viewer/src/platform.rs` aísla lo específico de cada SO (contexto de vídeo, renderizador, `FrameRef`). El modo `--file` sigue siendo solo de Windows.
- `decode_worker`: si un frame no clave lleva más de 250 ms en cola, se salta hasta el siguiente IDR (contador "saltados por retraso en cola") para no acumular retraso cuando la CPU no da abasto.
- Empaquetado: `scripts/package-linux.sh` (binarios sin símbolos) y job `linux` (ubuntu-22.04, glibc 2.35) en `release.yml`; el job Linux del CI compila y pasa clippy de visor, CLI, `cs-decode` y `cs-render`.

Pruebas en WSL2 (Ubuntu 24.04, `llvmpipe`, i5-1235U): decodificación sola 600/600 frames p50 10,9 ms; Windows → WSL 140/140 frames, 0 perdidos; con la cota de cola, espera p50 60 / p95 246 ms y reensamblado→Present p50 108 / p95 281 ms (entorno no representativo: Vulkan por software y CPU compartida). Entorno WSL: distro `Ubuntu-24.04`, repo copiado con `rsync` a `/root/crossscreen`, datos en `/root/csdata`.

**F7 (portapapeles) integrado** en emisor (`cs-pipeline`) y receptor (`crossscreen-viewer`) mediante `cs_clipboard::ClipboardBridge`: un hilo propio que sondea cada 250 ms y aplica textos remotos sin eco. **Desactivado por defecto** (puede contener contraseñas): se activa con `--clipboard` en `stream` y en el visor, o con la casilla del panel, en los dos equipos. Solo se sincronizan los cambios posteriores al inicio de la sesión. **Falta probarlo entre los dos equipos** (en un solo PC ambos lados comparten portapapeles).

**La escritura en el portapapeles real no se ha probado:** el portapapeles del dueño tenía contenido con formato y las pruebas se omiten solas para no destruirlo. Prueba pendiente (con texto plano copiado): `cargo test -p cs-clipboard --test windows_clipboard -- --ignored --test-threads=1`.

## Próximos pasos (en orden)
1. ✅ Validado por el dueño entre los dos equipos con `v0.0.1-alpha.8`: monitor extendido, entrada remota (F6), portapapeles (F7) y perfiles de red desde el panel.
2. Probar la transmisión iniciada desde el panel Tauri (`crossscreen-desktop` + `crossscreen-agent`) entre los dos equipos, incluidos los **perfiles de red** (Auto, Wi-Fi, LAN, Ahorro) y la entrada remota (F6).
3. Probar **por cable** para separar la latencia de red del Wi-Fi de la del pipeline.
4. Desactivar el ahorro de energía del Wi-Fi en los dos equipos para reducir los picos ocasionales de RTT (120–236 ms).
5. **Latencia:**
   - codificador más rápido (oneVPL/QSV directo; NVENC/AMF en otro hardware);
   - control de tasa dinámico (el MFT de Intel ignora el cambio de bitrate en caliente).
6. Prueba de estabilidad de 2 h; suspensión/reanudación con el monitor extendido.
7. **Linux → Windows (F4b):** validar el emisor experimental de duplicación (Portal/PipeWire → OpenH264 → QUIC) en GNOME Wayland; después añadir entrada, portapapeles, VAAPI y el modo extender.

## Problemas conocidos y decisiones vigentes
- **Versiones mezcladas:** solo puede haber un agente por usuario; un panel nuevo se conectaba a un agente viejo en marcha. Desde `alpha.8` el agente anuncia `versión+build` (`CROSSSCREEN_BUILD_ID`, lo fija `scripts/package-windows.ps1`) y el panel sustituye un agente de otra build (petición IPC `Shutdown`, rechazada si hay una sesión activa). Los mensajes de control desconocidos se ignoran en lugar de cortar la sesión, y cada lado avisa si el otro tiene una versión menor más antigua (protocolo 0.3).
- **VDD:** no usar su tubería. Modos y cantidad de monitores se cambian con el helper (XML + `pnputil /restart-device`, admin); mostrar y retirar con CCD (sin admin). Windows pone cada monitor nuevo en *duplicar*.
- **Codificador MF de Intel:**
  - necesita `timeBeginPeriod(1)` y excluir el proceso de la limitación de energía;
  - ignora el cambio de bitrate en caliente y también `CODECAPI_AVEncVideoGradualIntraRefresh` (ambos devuelven `S_OK` pero no alteran el flujo H.264);
  - CBR rellena cada frame (~40 KB a 20 Mbps; se escala automáticamente a ~8,1 Mbps para `1366×768`, dejando los P-frames en ~16,4 KiB).
- **Visor:** D3D11 directo en Windows (no wgpu), sin copias; por defecto decodifica a través de pérdidas (`DecodeThrough`), usa `KeyframeRequester` calmado (`Calm`) e inicializa `MF_MT_FRAME_SIZE` con el modo de `StartStream`.
- **Captura:** una sola duplicación por monitor y por proceso; el nombre GDI (`\\.\DISPLAYn`) cambia, así que hay que buscarlo siempre con `cs-display`.
- **Linux:** el receptor (visor + CLI) se ejecutó en WSL2; agente, IPC y panel de Linux compilan pero nunca se han ejecutado. En Linux el visor ignora `--clipboard` (avisa y sigue).
- **Clave privada:** DPAPI en Windows; en Linux un archivo 0600 (Secret Service pendiente).

## Bitácora
| Fecha | Quién | Qué |
|---|---|---|
| 2026-11-08 | Coordinador | Análisis de las 5 referencias, arquitectura, ADR, roadmap y PoC (docs/). |
| 2026-11-08 | Coordinador | Núcleo: cs-core/protocol/security/transport/discovery/session + CLI; 55 tests. |
| 2026-11-08 | Coordinador | PoC-1: VDD instalado; tubería descartada (lo tumba); topología CCD; 100/100 ciclos. |
| 2026-11-08 | 5 agentes en paralelo | PoC-2 captura, PoC-3 codificación, PoC-5 visor, agente + IPC + panel Tauri, *pacing* + DPAPI. |
| 2026-11-08 | 2 agentes + coordinador | MVP F2: `crossscreen stream` + `crossscreen-viewer --listen`; arreglo de frames perdidos por completo en el reensamblador; mismo equipo p50 29 ms. |
| 2026-11-08 | Coordinador | Repo en GitHub, CI en verde, release `v0.0.1-alpha.1`; revocación de pares en caliente en el visor. |
| 2026-11-08 | Dueño + coordinador | **Primera prueba real A → B por Wi-Fi:** funciona, pero con lag y congelaciones por pérdidas en ráfaga. Lanzados 2 agentes de resiliencia (ver "Trabajo en curso"). |
| 2026-11-08 | Agente integrador | Integración de resiliencia Wi-Fi en receptor (`DecodeThrough`, `KeyframeRequester` calmado, `SeqTracker::note_lost`) y emisor/transporte (FEC IDR 0,5, entrelazado de bloques, `AdaptiveFec`, filtro `<1 RTT`, inyector Gilbert-Elliott y sonda `GradualIntraRefresh`); mediciones documentadas y release `v0.0.1-alpha.2`. |
| 2026-11-08 | Dueño + agente | **Validación en Wi-Fi real (`PC-A → PC-B`, 60 s, `1366×768@60`):** corregido bloqueo de *pacing* (`paced = total - avail` y `set_rate_at`), escalado CBR por resolución (`8 094 kbps`), tope de ritmo para IDR, sondeo Win32 del cursor a 125 Hz + deduplicación de formas y tamaño inicial en `VideoDecoder`; pérdidas bajan de 31 a 2 frames y `RequestKeyframe` de 104 a 1. |
| 2026-11-08 | Agente | **Integración de streaming y modo receptor en agente y panel Tauri:** `StartStream`/`StopStream` y `StartReceiver`/`StopReceiver` en `cs-ipc` (`v1.1`), ejecución de `cs_pipeline::run_sender` y gestión del proceso `crossscreen-viewer` con anuncio mDNS de `session_port` en `crossscreen-agent`, vista «Transmitir o recibir pantalla» en `apps/desktop` e inclusión de `crossscreen-desktop.exe` en el paquete `.zip` de Windows. |
| 2026-11-08 | 2 agentes en paralelo | **Reconexión automática QUIC (F5) y entrada remota / KVM (F6):** ventana de gracia de 15 s en `cs-pipeline` que conserva vivo el monitor virtual y el hilo de captura mientras reconecta con el receptor; protocolo `ControlMessage::Input`, crate `cs-input` (`SendInput` en coordenadas del escritorio virtual + liberación de teclas/botones en `Drop` y en cortes de red) y captura de ratón/teclado en `crossscreen-viewer` integrados con `cs-pipeline`. |
| 2026-11-08 | Coordinador | **F6 y F7, piezas base:** `InputEvent` en el protocolo y crate `cs-input` (inyección verificada píxel a píxel en Windows real, también en modo DPI-aware); `ControlMessage::ClipboardText` (límite de mensaje de control subido a 1 MiB + 64 KiB) y crate `cs-clipboard` (`ClipboardSync` sin eco, backend Win32 con ventana propietaria). |
| 2026-11-08 | Coordinador | Retomado el trabajo de otro agente que se quedó sin cuota: **perfiles de red** en `cs-ipc`/agente/panel (faltaba un import que rompía `crossscreen-agent`) y arreglo de un *cast* en `cs-pipeline` que tenía el **CI de `main` en rojo** desde `6f34136`. 202 tests en verde. |
| 2026-11-08 | Coordinador | **F7 integrado:** `ClipboardBridge` en `cs-clipboard` (hilo Win32 + canales, `prime` para no enviar lo copiado antes de conectar), mensajes `ClipboardText` en el bucle del emisor y del receptor, opciones `--no-clipboard` y contadores en los resúmenes. Arreglo de *binary planting* en el panel (el agente solo se busca junto al ejecutable en release). 204 tests en verde. |
| 2026-11-08 | Coordinador | Revisión de seguridad de F7: portapapeles **opcional** (desactivado por defecto; `--clipboard` y casilla en el panel, llevado por `cs-ipc` hasta el visor); ranura "el último gana" en lugar de cola sin límite para textos remotos; `prime_unread` evita enviar lo copiado antes de conectar si la primera lectura falló. Release `v0.0.1-alpha.7`. 209 tests en verde. |
| 2026-11-08 | Dueño + coordinador | **Bug real: la sesión se cerraba al mover el ratón en PC-B.** Causa: el panel `alpha.7` usó un agente viejo (anterior a F6) que seguía en marcha; el emisor no entendía `ControlMessage::Input` (`Serde Deserialization Error`) y cortaba. Arreglos: mensajes desconocidos ignorados sin perder la sincronía del canal, protocolo 0.3 con aviso de funciones ausentes, `BUILD_ID` en el agente y sustitución automática desde el panel (IPC 1.2 `Shutdown`). Release `v0.0.1-alpha.8`. |
| 2026-11-08 | Dueño | **Validación con `alpha.8` entre PC-A y PC-B:** monitor extendido, ratón y teclado remotos, portapapeles en ambos sentidos y transmisión desde el panel funcionan correctamente. |
| 2026-11-08 | Coordinador | **Receptor Linux (F4, Windows → Linux):** OpenH264 de Cisco cargado en tiempo de ejecución en `cs-decode`, renderizador `wgpu` en `cs-render`, capa `platform` en el visor, salto a IDR por retraso en cola, `fetch-openh264.sh`, `package-linux.sh`, guía `docs/11-receptor-linux.md` y job Linux en CI/Release. Probado en Ubuntu 24.04 sobre WSL2 (140/140 frames) y validado por el dueño en Ubuntu real con el flujo Windows → Ubuntu. Release `v0.0.1-alpha.9`. |
| 2026-11-08 | Copilot SDK en VS Code | Emisor Linux experimental: Portal/PipeWire entrega I420, OpenH264 produce Annex-B y `cs-pipeline` reutiliza handshake QUIC, FEC y pacing para `duplicate`. Compila en el workspace Windows; la validación nativa requiere Ubuntu con PipeWire de desarrollo y una prueba real contra Windows. |
| 2026-11-08 | Coordinador | Validación del estado heredado: `cargo test --workspace` y `apps/desktop/npm.cmd run build` en verde. Actualizada la guía del panel para documentar la transmisión, recepción, perfiles de red y portapapeles opcional ya integrados. |
