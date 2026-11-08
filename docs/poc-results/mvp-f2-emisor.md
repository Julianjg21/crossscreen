# MVP F2: emisor (`crossscreen stream`)

**Estado:** funciona de punta a punta en este equipo: monitor virtual → captura DXGI → H.264 Intel QSV (MF) en el mismo `ID3D11Device` → datagramas QUIC con FEC y *pacing* + canal de control. Probado contra un receptor de pruebas propio (`null_sink`) y contra el receptor real (`crossscreen-viewer --listen`, en desarrollo en paralelo): el visor muestra el contenido del monitor virtual con el cursor.
**Pendiente:** objetivo de latencia de codificación (≤ 6 ms p95) no alcanzado con MF en la Iris Xe (igual que en la PoC-3), cambio de modo a mitad de sesión sin probar, pruebas entre dos equipos reales (ver al final).
**Fecha:** 2026-11-08.

**Entorno:** el mismo de las PoC-2/3: Windows 11 Pro 10.0.26200, portátil con una sola GPU (Intel Iris Xe), panel 1920×1080 al 125 %, VDD (`MTT1337`). Compilación `release`. Durante las medidas había otras compilaciones en paralelo (el otro agente), así que son cifras de un equipo cargado, no de laboratorio.

**Contrato:** `docs/09-contrato-sesion-mvp.md`, seguido al pie de la letra.

## Código

| Fichero | Qué hace |
|---|---|
| `crates/cs-pipeline/src/lib.rs` | API reutilizable: `SenderConfig`, `run_sender(conn, cfg, shutdown) -> SenderReport`. `crossscreen-agent` podrá llamarla con su propia conexión y señal de parada |
| `crates/cs-pipeline/src/session.rs` | Canal de control (Hello ⇄ Hello, SinkCapabilities, StartStream → StreamStarted, bucle de sesión, Bye), monitor virtual, arranque/parada del hilo y tarea de envío de vídeo |
| `crates/cs-pipeline/src/win/worker.rs` | Hilo de captura + codificación |
| `crates/cs-pipeline/src/win/priority.rs` | MMCSS "Games" + `AVRT_PRIORITY_HIGH` (o `THREAD_PRIORITY_HIGHEST`) |
| `crates/cs-pipeline/src/win/console.rs` | Reactiva Ctrl+C si el proceso lo heredó desactivado |
| `crates/cs-pipeline/src/display_guard.rs` | Guardia que retira el monitor virtual en `Drop`; elección del modo |
| `crates/cs-pipeline/src/cursor.rs` | Forma del cursor → RGBA ≤ 128×128; posición del *hotspot* |
| `crates/cs-pipeline/src/timing.rs` | `pts` del contrato y coalescencia de `RequestKeyframe` |
| `crates/cs-pipeline/src/stats.rs` | Métricas finales |
| `crates/cs-pipeline/examples/null_sink.rs` | Receptor de pruebas desechable: habla el contrato, reensambla y valida el H.264 sin decodificar |
| `apps/cli/src/stream.rs` | Subcomando `crossscreen stream` (envoltura fina) |

```text
crossscreen stream <ip:47810> --peer <huella|prefijo> [--data-dir D] [--mode extend|duplicate]
    [--width W --height H --hz F] [--mbps 20] [--fec 0.1] [--pacing 0.6|off]
    [--pacing-wait highres|thread|yield|tokio] [--seconds S] [--static-refresh 1.0]
```

## Diseño

### Secuencia
1. Conexión QUIC con `PeerPolicy::Exactly(huella)` y ALPN `crossscreen/0`. Se abre el stream bidireccional de control (`FramedStream`).
2. `Hello` → espera `Hello` (versión mayor distinta: `Bye { IncompatibleVersion }` y error) → espera `SinkCapabilities`. Si el receptor no anuncia H.264, `Bye { Error }`.
3. **Modo del monitor virtual**: el pedido por CLI si se da alguno de `--width/--height/--hz`; si no, el `native_mode` del receptor, luego ese tamaño a 60 Hz y, por último, 1920×1080@60. Solo se salta al siguiente candidato si el driver responde "modo no disponible". En `duplicate` no se crea monitor: se captura la pantalla principal.
4. Se arranca el hilo de captura + codificación **antes** de `StartStream`: así un fallo de captura o del codificador se detecta y se notifica (`Bye { Error }`) antes de empezar. El hilo captura pero no codifica hasta `StreamStarted`.
5. `StartStream { stream_id: 1, mode, H264, display_mode, bitrate_kbps }`. `display_mode` es el modo aplicado (en `duplicate`, el tamaño físico que da la duplicación).
6. Espera `StreamStarted` (15 s). A partir de ahí el primer frame es IDR (`force_idr`), también si el escritorio está estático (se recodifica la última imagen).

### Hilos y colas
- **Hilo de captura + codificación** (`cs-capture-encode`), con MMCSS "Games" y prioridad alta. Hace `DxgiCapture::next_frame(1/fps)`, publica el cursor y codifica con `MfH264Encoder` sobre `DxgiCapture::device()`.
- **Entrada del codificador:** la textura propia de `cs-capture` (copia `CopyResource` de la superficie de DDA, uso DEFAULT, `SRV | RTV`, BGRA8). El Video Processor la acepta como entrada sin más copias.
  - No se probó a pasar la superficie de DDA directamente. Habría que retener el frame adquirido durante la conversión, lo que retrasa a DWM; la copia en GPU es barata (PoC-2).
- **Canal acotado de capacidad 1** hacia la tarea de envío. Antes de codificar se mira si hay hueco. Si no lo hay, la red no da abasto y el frame se descarta **sin codificar**. Así no se acumula cola y la cadena de referencias sigue intacta (el siguiente P referencia al último enviado). Si una unidad ya codificada no cupiera, se fuerza un IDR (contador `tras codificar`, que fue 0 en todas las pruebas).
- **Tarea de envío** (tokio): `Packetizer::new(1, { max_datagram: conn.max_datagram_size(), fec })`, `Pacer` en modo *spread* y `send_paced` con temporizador de alta resolución. `frame_seq` es consecutivo desde 0 entre los frames enviados.
- **Bucle de control** (tokio `select!`): señal de parada, `--seconds`, mensajes del receptor, cambios de cursor (canales `watch`, gana el último valor), IDR diferido y fin del hilo.

### Detalles del contrato
- **`pts_us`:** µs de QPC módulo 2³².
  - Se usa `LastPresentTime` de DXGI si no tiene más de 100 ms de antigüedad respecto a la adquisición; si no, el QPC de la adquisición.
  - El primer frame tras crear la duplicación trae una presentación antigua: se midieron 754 ms con el escritorio estático, que daría una "latencia" falsa.
  - Hacia el codificador se pasan los µs completos, forzados a ser estrictamente crecientes, porque el MFT empareja la entrada con la salida por `pts`.
- **`RequestKeyframe`:** como máximo un IDR forzado cada 100 ms.
  - Una petición dentro de la ventana no se pierde: queda diferida hasta que vence.
  - Si no llegan frames nuevos (escritorio estático), el IDR se genera recodificando la última imagen en el siguiente intervalo.
- **Cursor:**
  - `CursorShape`: solo cuando cambia la forma. Se convierte a RGBA con `CursorShape::to_rgba()` de `cs-capture`, se recorta a 128×128 y se acota el *hotspot*.
  - `CursorPosition`: cuando cambian la posición o la visibilidad, como mucho una vez por llamada a `next_frame`, es decir, ≤ la frecuencia de refresco.
  - **Convención:** se envía el punto del *hotspot* (esquina de DXGI + *hotspot*). El receptor dibuja en `pos - hotspot`, y así lo hace el visor (`cs-render`).
  - Con el escritorio estático, los movimientos del ratón se recogen al agotarse el *timeout* de `next_frame`, así que pueden llegar con hasta 1/fps de retraso.
- **Escritorio estático:** no se envía nada nuevo, con una excepción opcional, `--static-refresh` (1 s por defecto, `0` lo desactiva): cada segundo se recodifica la última imagen como un P.
  - Sirve para que el receptor detecte la pérdida del último frame (hueco en `frame_seq`) y pida un IDR. Sin esto, una pérdida justo antes de quedarse quieto el escritorio dejaría la imagen rota indefinidamente.
  - **Coste:** con CBR y VBV de 1 frame, el MFT de Intel rellena cada frame hasta unos 40 KiB a 20 Mbps (ver los hallazgos), así que el refresco cuesta unos 0,33 Mbps.
- **`ACCESS_LOST`:** lo recupera `cs-capture` dentro de `next_frame` (se vieron 2 en los primeros segundos tras mostrar el monitor y se recuperaron solos).
  - Si cambia el tamaño o el `device_generation`, el codificador se recrea y el primer frame sale como IDR.
  - Si la salida no vuelve en 5 s, el error es fatal y la sesión termina.
- **Errores del codificador:** se cuentan y el frame se pierde.
  - Cada 3 errores seguidos se recrea el codificador; con 9 seguidos la sesión termina con un mensaje claro.
  - Si el codificador no se puede crear al principio, se informa antes de `StartStream`.
- **Fin de sesión:** Ctrl+C, Ctrl+Break, cierre de la consola, `--seconds`, `Bye` o `StopStream` del receptor, conexión perdida (el *idle timeout* de QUIC es de 10 s) o fallo fatal de captura o codificación. En todos los casos:
  1. `Bye` (si el par sigue).
  2. Se para y se espera el hilo.
  3. Se vacía el envío.
  4. **Se retira el monitor virtual.**
  - La guardia (`VirtualDisplayGuard`) lo retira también en `Drop`, de modo que cubre `?`, errores y pánicos con *unwind*.
  - Lo único que no cubre es matar el proceso a la fuerza (`TerminateProcess`). En ese caso queda `crossscreen display hide`.

## Cómo se verificó

Dos `--data-dir` en el scratchpad, emparejados con `pair-listen` / `pair` por loopback.

- **Contenido:** una ventana propia de pruebas (WinForms desde PowerShell, DPI-aware), colocada sobre el monitor virtual, que repinta a ~60 Hz:
  - fondo de color variable;
  - una barra blanca que se mueve;
  - el texto "CrossScreen F2 frame N";
  - opcionalmente, mueve el cursor sobre el monitor virtual y lo devuelve a su sitio al acabar.
- **Receptor de pruebas:** `null_sink` valida cada unidad:
  - código de inicio Annex-B y `forbidden_zero_bit`;
  - que el primer frame sea IDR;
  - SPS + PPS + *slice* IDR en cada IDR;
  - que la marca `keyframe` coincida con las NAL;
  - el SPS analizado.
- **Pérdidas y latencia:** `null_sink` puede simular pérdidas (`--drop-every N` descarta todos los datagramas de 1 de cada N frames) y pedir IDR periódicos. Mide la latencia captura → frame reensamblado con el mismo QPC.

### Resultados (1920×1080@60, 20 Mbps, FEC 0,1)

| Tanda | Captura CPU p50/p95 | DWM→acquire p50/p95 | Codificación p50/p95 (solo MFT) | Captura→último datagrama p50/p95 | Captura→reensamblado p50/p95/p99 | Notas |
|---|---|---|---|---|---|---|
| A: *pacing* 0,6, pérdidas simuladas | 0,31 / 0,76 ms | 0,29 / 0,70 ms | 11,62 / 13,83 ms (10,40 / 12,29) | 22,86 / 27,73 ms | 22,17 / 26,95 / 35,08 ms | 727 frames capturados; la ventana pintó 624 en 10 s |
| B: *pacing* 0,6, IDR cada 3 s + 1 de cada 150 frames perdido + cursor | 0,37 / 0,58 ms | 0,32 / 0,57 ms | 10,52 / 12,10 ms (9,37 / 10,92) | 21,80 / 23,40 ms | 21,29 / 22,67 / 26,83 ms | 786 capturados; la ventana pintó 803 en 13 s (≈ 60 fps capturados mientras hay animación) |
| C: *pacing* `off` | 0,58 / 0,78 ms | 0,48 / 0,84 ms | 12,35 / 14,87 ms (10,71 / 13,14) | 13,72 / 16,92 ms | 14,47 / 18,94 / 23,00 ms | 2 `ACCESS_LOST` al principio, recuperados |
| D: **visor real** en el mismo equipo | 0,43 / 0,83 ms | 0,85 / 7,04 ms | 13,79 / 21,97 ms (11,60 / 19,59) | 28,80 / 35,65 ms | — | la GPU la comparten el emisor, el visor y DWM |

**Tanda D, receptor real** (`crossscreen-viewer --listen --size 1280x720 --same-host-latency`, en ventana sobre el panel principal):

- **Red:** 669 frames enviados y 669 reensamblados, 0 perdidos, 0 errores de decodificación, *zero-copy* en el visor.
- **Latencias del visor:**
  - decodificación p50 1,14 / p95 2,94 ms;
  - **captura → `Present` devuelto: p50 32,1 / p95 46,5 / p99 58,9 ms** (mismo QPC).
- **Imagen:** la captura del *back buffer* del visor (frame 400) muestra la ventana de pruebas del monitor virtual ("CrossScreen F2 frame 285", la barra blanca) escalada a 1280×720, con el cursor en la posición esperada.
  - El `native_mode` anunciado, 1280×720, no existe en el driver, así que se usó 1920×1080@60 de reserva, como prevé el contrato.
- **Frames sin codificar:** 709 capturados frente a 669 codificados. Unos 38 frames se capturaron antes de `StreamStarted` (mientras el visor creaba la ventana y el decodificador) y no se codifican por diseño; 2 se descartaron por contrapresión.
- **Cursor:** 137 posiciones y 3 formas, recibidas y dibujadas.

**IDR y recuperación (tanda B):**
- `null_sink` envió 10 `RequestKeyframe`: 5 por huecos en `frame_seq` (frames simulados como perdidos) y 5 periódicos.
- El emisor atendió las 10 y salieron 11 IDR, contando el inicial.
- Petición → IDR recibido: p50 27,9 ms, máx. 37,8 ms.
- Validación correcta en todas las tandas:
  - 0 frames sin código de inicio, 0 `forbidden_bit`, 0 IDR sin SPS/PPS, 0 marcas IDR incoherentes;
  - SPS `profile 100, level 4.2, 1920x1080, refs 1, max_num_reorder_frames 0`;
  - NAL por frame: AUD, SEI, PPS y *slice*, más el SPS en cada IDR.

**Bitrate:**
- 15,2 Mbps de H.264 y 17,0 Mbps en datagramas (FEC + cabeceras) en la tanda A, de media sobre 16 s, de los que unos 5 s fueron de escritorio estático.
- Con animación continua, la tasa ronda los 20 Mbps a 60 fps (tanda C: 17,7 Mbps a 53 fps de media).
- IDR medio de 65–71 KiB y P medio de 40,6 KiB.

**Contrapresión:** en la tanda B se descartaron 13 frames antes de codificar (1,6 %) con el *pacing* activo y 1 sin él. Cuando la ventana de *pacing* del frame anterior y la codificación del siguiente se solapan con algo de *jitter*, el canal de capacidad 1 está lleno y el frame se descarta, en lugar de esperar y sumar latencia.

### Otras pruebas
- **`--mode duplicate`:** captura de `\\.\DISPLAY1` a 1920×1080 físicos, flujo válido. La primera versión mostraba 1536×864 en un mensaje, porque `desktop_rect` llega en píxeles lógicos (la CLI no es DPI-aware); ya se usa el tamaño de la duplicación.
- **Ctrl+C real** (`CTRL_C_EVENT` con `GenerateConsoleCtrlEvent` en una consola aparte): parada ordenada y monitor retirado.
  - Antes de `enable_ctrl_c()`, Ctrl+C **no llegaba**: el proceso heredaba el atributo "ignorar Ctrl+C" del lanzador (Git Bash).
  - Ctrl+Break sí llegaba y también se probó.
- **Conexión perdida** (`taskkill /F` del receptor): la sesión terminó unos 12 s después ("conexión perdida", *idle timeout* de 10 s) y el monitor se retiró.
- **`--seconds`, `Bye` del receptor:** probados en todas las tandas. Tras cada prueba, `crossscreen display list` muestra el VDD con `activo=false`.

## Hallazgos

1. **El dispositivo de `cs-capture` sirve tal cual para el codificador.** Se crea solo con `BGRA_SUPPORT`, sin `VIDEO_SUPPORT`, y aun así el Video Processor y el MFT de Intel funcionan sobre él. No hizo falta tocar `cs-capture`.
2. **La copia de DDA (DEFAULT, `SRV | RTV`) es una entrada válida del Video Processor.** La duda abierta de la PoC-3 queda resuelta para esta vía.
3. **CBR con VBV de 1 frame rellena cada frame** (P de ~40 KiB a 20 Mbps aunque la imagen no cambie). La tasa real es proporcional a los fps entregados, y cada refresco del escritorio estático cuesta un frame completo.
4. **La codificación sigue en 10–12 ms p50 y 12–15 ms p95** (14–22 ms con el visor compitiendo por la GPU). Es la mayor partida del presupuesto, como predecía la PoC-3. Con *pacing* 0,6 el último datagrama sale unos 8–10 ms después de codificar: es el coste del reparto (21 frente a 14 ms p50 en loopback).
5. **`LastPresentTime` del primer frame tras crear la duplicación es antiguo** (754 ms medidos): se acota a 100 ms.
6. **`Reassembler` no emite `Lost` para un frame del que no llegó ningún datagrama.** El receptor tiene que detectar el hueco en `frame_seq`; el visor ya lo hace ("0 sin ningún datagrama") y `null_sink` también.
7. **Ctrl+C puede llegar desactivado por herencia:** la CLI lo reactiva con `SetConsoleCtrlHandler(NULL, FALSE)`.
8. **Prioridad:** MMCSS "Games" se obtuvo sin privilegios en todas las ejecuciones (el *log* `info` muestra `MmcssGames`).

## Cambios en otros crates
Ninguno. No se encontraron fallos en `cs-capture` ni en `cs-encode` que hubiera que corregir.

## Pendiente
1. **Latencia de codificación:** QSV directo (oneVPL) o NVENC/AMF en equipos con GPU dedicada (ver PoC-3).
2. **Cambio de modo durante la sesión:** no hay mensaje en el contrato para anunciar el nuevo `display_mode`.
   - El emisor recrea el codificador y manda un IDR con el SPS nuevo; el receptor debe reconfigurarse al ver el SPS.
   - Sin probar: `cs-display` no expone cambiar el modo de un monitor ya activo y `display show` busca uno inactivo.
3. **Pruebas entre dos equipos** (Wi-Fi y cable): la latencia captura → presentación solo se puede medir así con un reloj común.
4. **Adaptación de tasa:** el MFT ignora el cambio de tasa en caliente (PoC-3). Hoy `ReceiverStats` solo se registra.
5. **Escritorio seguro, suspensión y *watchdog*** de "sin frames con contenido cambiando" (PoC-2, pendientes 4 y 6).
6. **Rutas de error del codificador** (recreación tras 3 errores, fatal con 9): implementadas pero no provocadas en las pruebas.
