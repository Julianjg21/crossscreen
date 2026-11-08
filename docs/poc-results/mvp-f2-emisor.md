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
- En la primera entrega no hizo falta tocar `cs-capture` ni `cs-encode`.
- En la iteración de resiliencia en Wi-Fi se ampliaron `cs-transport` (`fec`, `simloss`, `Packetizer` con paridad separada para IDR y entrelazado de bloques), `cs-encode` (`EncoderConfig::intra_refresh`, `h264::parse_sei_messages` y sonda `--intra-refresh` en `encode_bench`) y `cs-pipeline` / `apps/cli`.

## Iteración de resiliencia en Wi-Fi y verificación de *gradual intra refresh*

### Motivación (primera prueba Wi-Fi entre 2 equipos)

En la sesión de 60 s por Wi-Fi (`v0.0.1-alpha.1`, `1366×768@60`), el emisor recibió **104 `RequestKeyframe`** y forzó **90 IDR** (media **79,5 KiB**, ~68 datagramas en 2 bloques FEC, frente a **36,7 KiB** por P-frame). Con FEC fijo de **0,10** y envío secuencial bloque a bloque:
- Un bloque de 48 shards de datos solo llevaba **5 shards de paridad**: cualquier ráfaga Wi-Fi de ≥ 6 datagramas consecutivos rompía el bloque y hacía perder el frame entero.
- Como los IDR ocupaban más del doble de datagramas que un P-frame pero tenían el mismo 10 % de paridad, eran los más vulnerables a las ráfagas Wi-Fi, realimentando la tormenta de `RequestKeyframe`.
- Además, cuando el RTT subía (943 ms iniciales en esa sesión), llegaban varios `RequestKeyframe` disparados antes de que el receptor pudiera ver el IDR que ya estaba en vuelo.

### Mejoras implementadas en `cs-transport`, `cs-pipeline` y `apps/cli`

1. **Paridad FEC separada para IDR (`Packetizer::with_keyframe_fec_ratio`, `--keyframe-fec`, por defecto `0.5`):**
   - Los frames marcados como `keyframe` usan `max(keyframe_fec_ratio, fec_ratio)` (50 % por defecto: un IDR de 65 shards repartido en dos bloques de 33 + 32 lleva **17 + 16 = 33 shards de paridad** en vez de 4 + 4 = 8).
2. **Entrelazado de shards entre bloques FEC (`Packetizer::with_interleave`, activo por defecto; `--no-fec-interleave` para desactivar):**
   - Cuando un frame se divide en $B > 1$ bloques FEC (cualquier frame > 48 shards, ~55 KiB), los datagramas se emiten intercalando bloques (`(0,0), (1,0), …, (0,1), (1,1), …`) en lugar de enviar todo el bloque 0 y luego el bloque 1.
   - Así, una ráfaga de $L$ datagramas consecutivos perdidos reparte el daño en $\approx L / B$ shards por bloque, duplicando o triplicando la longitud de ráfaga que el código Reed-Solomon absorbe sin subir el bitrate (verificado con test determinista en `cs-transport::reassembly`: ráfaga de 15 datagramas sobre un frame de 3 bloques con 6 shards de paridad por bloque se pierde al 100 % sin entrelazar y se recupera al 100 % entrelazado).
3. **FEC adaptativo para P-frames (`cs_transport::fec::AdaptiveFec`, activo por defecto; `--no-adaptive-fec` para fijarlo):**
   - Parte de `--fec` (**`0.15`** por defecto, antes `0.10`) y ajusta la paridad de los P-frames al recibir cada `ReceiverStats` (~1 s):
     - Si `frames_lost` crece, sube de forma multiplicativa (`×1,5`, mínimo `+0,05`) y asegura al menos `2,5 × pre_fec_loss`.
     - Si no hay frames perdidos pero `pre_fec_loss_bp ≥ 200` (≥ 2 % de shards recuperados por FEC), sube preventivamente `+0,05`.
     - Tras **5 intervalos limpios consecutivos** (`frames_lost == 0` y `pre_fec_loss_bp < 50`), baja gradualmente `-0,02` hasta un suelo de `0,08` (tope `0,50`).
4. **Filtro de `RequestKeyframe` dentro de `< 1 RTT` (`KeyframeCoalescer::request_with_rtt`):**
   - Registra el instante de cada IDR emitido (`note_forced`, incluido el inicial y los periódicos).
   - Toda petición `RequestKeyframe` que llegue antes de `min(conn.rtt(), 500 ms)` desde el último IDR se **ignora por completo** (sin dejar un IDR diferido en cola), porque corresponde a pérdidas anteriores a que ese IDR llegara al receptor.
   - Entre `1 RTT` y `100 ms` se sigue coalesciendo en un único IDR diferido al cumplirse los `100 ms`.
5. **Inyector de pérdidas reproducible en el emisor (`cs_transport::simloss`, `--sim-loss`, `--sim-loss-seed`):**
   - Soporta `bernoulli:p=0.02` y `gilbert:p=0.015,r=0.25,h=0.8` directamente en `crossscreen stream` para ensayar canales Wi-Fi degradados sin herramientas externas.

### Verificación experimental de `CODECAPI_AVEncVideoGradualIntraRefresh` en el MFT de Intel

Se añadió `EncoderConfig::intra_refresh: Option<u32>` en `cs-encode` (que configura `CODECAPI_AVEncVideoGradualIntraRefresh` como `VT_UI4` antes de fijar los tipos de entrada/salida y lee el valor devuelto con `GetValue`), el parser `h264::parse_sei_messages` y el modo `cargo run --release -p cs-encode --example encode_bench -- --intra-refresh`.

Resultados medidos sobre **Intel(R) Iris(R) Xe Graphics** (`ven_8086`, MFT H.264 hardware) codificando **120 frames deterministas a 1080p20**:

| `GOP` | Valor pedido (`intra_refresh`) | `SetValue` | `GetValue` leído | IDR / P emitidos | P media / p95 / máx. | Mensajes SEI `(tipo, bytes)` | AU idénticas byte a byte respecto a `None (0)` |
|---:|---|---|---|---:|---|---|---:|
| 600 | `None (0)` | — | `Some(0)` | 1 / 119 | 125,0 / 128,4 / 199,6 KiB | `(0, 12): 1`, `(1, 9): 120` | 120 / 120 |
| 600 | `1 (0x00000001)` | `Ok(())` | `Some(1)` | 1 / 119 | 125,0 / 128,4 / 199,6 KiB | `(0, 12): 1`, `(1, 9): 120` | **120 / 120** |
| 600 | `30 (0x0000001E)` | `Err(0x80070057)` (`E_INVALIDARG`) | — | — | — | — | — |
| 600 | `65537 (0x00010001)` | `Ok(())` | `Some(65537)` | 1 / 119 | 125,0 / 128,4 / 199,6 KiB | `(0, 12): 1`, `(1, 9): 120` | **120 / 120** |
| 600 | `0x0001001E`, `0x0001003C`, `0x0002001E` | `Err(0x80070057)` (`E_INVALIDARG`) | — | — | — | — | — |
| 30 | `None (0)` | — | `Some(0)` | 4 / 116 | 124,0 / 127,9 / 212,2 KiB | `(0, 12): 4`, `(1, 9): 120` | 120 / 120 |
| 30 | `1 (0x00000001)` | `Ok(())` | `Some(1)` | 4 / 116 | 124,0 / 127,9 / 212,2 KiB | `(0, 12): 4`, `(1, 9): 120` | **120 / 120** |
| 30 | `65537 (0x00010001)` | `Ok(())` | `Some(65537)` | 4 / 116 | 124,0 / 127,9 / 212,2 KiB | `(0, 12): 4`, `(1, 9): 120` | **120 / 120** |

**Conclusión:** al igual que ocurre con el cambio de bitrate en caliente y con `AVEncMPVDefaultBPictureCount` (PoC-3), el wrapper MFT de Intel acepta los valores `1` y `0x00010001` en `CODECAPI_AVEncVideoGradualIntraRefresh` sin error y los conserva en `GetValue`, pero **es un *no-op* total**: las 120 unidades de acceso generadas son **100 % idénticas byte a byte** a las codificadas sin la propiedad, solo se emiten SEI `buffering_period (0)` y `pic_timing (1)` (nunca `recovery_point (6)`), y con `GOP = 30` sigue insertando los 4 IDR periódicos. Para disponer de *intra refresh* real en Intel habrá que ir a oneVPL/QSV directo (`mfxExtCodingOption2::IntRefType`). La opción `--intra-refresh` se deja expuesta en `crossscreen stream` y documentada para pruebas con otros MFT (AMD/NVIDIA).

### Comparativa medida del emisor bajo pérdidas en ráfaga Gilbert-Elliott

Pruebas de **15 s (900 frames a 60 fps)** en *loopback* con el flujo `bench_1080p.h264` (1920×1080, 20 Mbps) bajo el mismo canal Gilbert-Elliott (`p = 0,015`, `r = 0,25`, `h = 0,8`, `seed = 24301`, **~4,75 % de datagramas perdidos en ~508–587 ráfagas** de media 4 datagramas), comparando contra `null_sink` y extremo a extremo contra `crossscreen-viewer`:

| Prueba (15 s, 900 frames enviados) | Datagramas enviados / descartados | Shards recup. FEC | Frames entregados (fps) | Frames perdidos post-FEC | `RequestKeyframe` recibidos | IDR forzados (total IDR) | Respuesta petición → IDR (p50 / p95 / máx.) | Evolución `AdaptiveFec` |
|---|---:|---:|---:|---:|---:|---:|---|---|
| **Emisor antiguo → `null_sink`** (FEC 0,10 fijo, sin entrelazar, sin filtro RTT) | 32 434 / 1 632 (4,79 %) | 472 | 756 (50,4 fps) | **144** (16,0 %) | **144** | **114** (87 recibidos) | 17,6 / 84,2 / 150,0 ms | fijo `0,10` |
| **Emisor nuevo → `null_sink`** (FEC P=0,15 adaptativo, IDR=0,50, entrelazado, filtro RTT) | 37 946 / 1 891 (4,75 %) | **1 082** | **886 (59,1 fps)** | **14** (1,6 %) | **14** | **12** (17 recibidos) | **17,3 / 23,8 / 23,8 ms** | `0,15 → 0,23 → 0,34 → 0,50` (3 ajustes) |
| **Emisor nuevo → `crossscreen-viewer` nuevo** (extremo a extremo) | 37 946 / 1 891 (4,75 %) | **1 082** | **886 (59,0 fps)** | **14** (1,6 %) | **12** | **12** | **18,1 / 22,1 / 22,1 ms** | `0,15 → 0,23 → 0,34 → 0,50` (3 ajustes) |

Resumen del impacto:
- Los **shards recuperados por FEC se más que duplican** (de 472 a **1 082**) gracias al entrelazado de bloques, a la paridad del 50 % en los IDR y a la subida automática de `AdaptiveFec` (`0,15 → 0,23 → 0,34 → 0,50` en los primeros 4 segundos al detectar pérdidas post-FEC).
- Los **frames perdidos post-FEC bajan un 90,3 %** (de 144 a **14 frames en 15 s**; una vez `AdaptiveFec` alcanza `0,50` en el segundo 4, solo se pierden 4 frames en los 11 segundos restantes).
- Los **IDR forzados caen un 89,5 %** (de 114 a **12**) y todos los episodios se recuperan a la primera en **17–18 ms p50 (< 24 ms máx.)**, frente a colas de hasta 150 ms cuando los propios IDR se perdían en las ráfagas.

### Pruebas reales en Wi-Fi entre dos equipos (`PC-A` → `PC-B`, 60 s, `1366×768@60`)

Al probar `v0.0.1-alpha.2` sobre el enlace Wi-Fi real (`DESKTOP-6K2V76S` → `DESKTOP-93VFE2P`, `1366×768@60`), la tormenta de `RequestKeyframe` bajó de 104 a 4, pero aparecieron cuatro cuellos de botella adicionales que se diagnosticaron y corrigieron en la misma sesión:

1. **Cursor a `10,6 Hz` (`638` posiciones en 60 s) y con saltos:**
   - `DxgiCapture::next_frame` hacía `continue` al recibir actualizaciones de solo puntero (`LastPresentTime == 0`) hasta agotar los `16,67 ms` del *timeout*, y el hilo no podía consultar el cursor mientras `MfH264Encoder::encode` tardaba `7–52 ms`.
   - **Corrección:** `next_frame` devuelve `Ok(None)` de inmediato en actualizaciones de puntero; se añadió `cs_capture::windows_dxgi::poll_cursor_in_rect` (`GetPhysicalCursorPos` + `GetCursorInfo`) sondeado cada **8 ms (125 Hz)** desde `session.rs` con independencia del codificador, y se deduplican los `CursorShape` idénticos.
2. **Relleno CBR de 20 Mbps en `1366×768` y micro-ráfagas de 124 Mbps en los IDR:**
   - Con `20 000 kbps` CBR sobre `1366×768`, el MFT de Intel rellenaba cada P-frame a `40,3 KiB` y cada IDR a `103,0 KiB` (`155 KiB` con 50 % de FEC), que `Spread { fraction: 0.6 }` inyectaba en `10 ms` (**124 Mbps**), desbordando la cola Wi-Fi (`15 frames perdidos sin ningún datagrama`, incluido el primer IDR forzado).
   - **Corrección:** `effective_bitrate_kbps` escala el objetivo CBR por defecto según el área respecto a `1920×1080` (`8 094 kbps` en `1366×768`, bajando los P-frames a `16,4 KiB` y los IDR a `25–31 KiB`) y `Pacer::with_max_rate` acota el ritmo máximo de `Spread` a `2,2 × bitrate`.
3. **Bloqueo de hasta 14 s en `Pacer::schedule` cuando un frame superaba `burst_bytes` por pocos bytes:**
   - `Pacer::schedule` restaba `capacity` (`4 648 B`) en vez del saldo real `avail` del *token bucket* y llamaba a `set_rate` antes de acreditar los tokens acumulados. Un frame de ~`4 650 B` tras un *bucket* vacío daba `paced = 2 B` (`rate = 200 B/s`) y programaba sus datagramas a ~14 s vista (`Captura→enviado máx 13 995,92 ms`).
   - **Corrección:** `TokenBucket::set_rate_at` acredita primero los tokens hasta `t0` y `Spread` calcula `paced = (total - avail).max(0.0)` (cubierto por el test `spread_never_stalls_when_frame_barely_exceeds_capacity`).
4. **Parada inicial de `456,8 ms` en `VideoDecoder` y tope de espera en `KeyframeRequester`:**
   - `VideoDecoder::new_with_size` fija `MF_MT_FRAME_SIZE` antes de `SetInputType` para no reservar primero superficies `1920×1080` y sufrir `MF_E_TRANSFORM_STREAM_CHANGE` en el primer IDR, `DECODE_QUEUE` sube de 16 a 64 y `KeyframeRequester::wait_for` acota `base` a `KEYFRAME_MAX_WAIT`.

Comparativa de las **tres sesiones reales de 60 s por Wi-Fi** (`PC-A` → `PC-B`, `1366×768@60`):

| Métrica (60 s reales por Wi-Fi) | 1.ª prueba (`v0.0.1-alpha.1`) | 2.ª prueba (`v0.0.1-alpha.2`) | 3.ª prueba (con correcciones de *pacing*, CBR y cursor 125 Hz) |
|---|---:|---:|---:|
| Frames capturados / enviados (fps) | 1 651 / 1 564 (26,1 fps) | 1 250 / 1 210 (20,2 fps) | **1 825 / 1 781 (29,7 fps)** |
| Tamaño medio IDR / P-frame | 79,5 KiB / 36,7 KiB | 103,0 KiB / 40,3 KiB | **31,3 KiB / 16,4 KiB** |
| Bitrate en datagramas (datagramas enviados) | 8,40 Mbps (58 063) | 8,71 Mbps (46 309) | **4,57 Mbps (25 382)** |
| `Captura→enviado (QPC)` p50 / p95 / máx. | 17,9 / 27,8 / 121,4 ms | 18,1 / 26,8 / 90,2 ms | **18,13 / 25,62 / 53,18 ms** |
| Frames perdidos post-FEC (sin ningún datagrama) | **31** (27) | **19** (15) | **2 (0)** |
| Frames descartados esperando IDR | **213** | **0** | **0** |
| `RequestKeyframe` enviados (por espera vencida) | **104** (—) | **4** (2) | **1 (0)** |
| IDR forzados (total IDR emitidos) | **90** (91) | **5** (6) | **2 (4)** |
| Recuperación `1.ª pérdida → IDR` | 400 – **6 400 ms** | 3 288 – 4 042 ms | **96,9 ms** (2.ª sesión) / **627,3 ms** (3.ª, RTT 120 ms) |
| Posiciones de cursor enviadas (repintados solo cursor) | 412 (—) | 638 (285) | **3 040 (846–1 122)** |
| *Jitter* RFC 3550 al final de la sesión | 38,8 ms | 38,9 ms | **10,7 – 12,5 ms** |

## Pendiente
1. **Latencia de codificación:** QSV directo (oneVPL) o NVENC/AMF en equipos con GPU dedicada (ver PoC-3).
2. **Cambio de modo durante la sesión:** no hay mensaje en el contrato para anunciar el nuevo `display_mode`.
   - El emisor recrea el codificador y manda un IDR con el SPS nuevo; el receptor debe reconfigurarse al ver el SPS.
   - Sin probar: `cs-display` no expone cambiar el modo de un monitor ya activo y `display show` busca uno inactivo.
3. **Pruebas entre dos equipos** (Wi-Fi y cable): la latencia captura → presentación solo se puede medir así con un reloj común.
4. **Adaptación de tasa:** el MFT ignora el cambio de tasa en caliente (PoC-3). Hoy `ReceiverStats` solo se registra.
5. **Escritorio seguro, suspensión y *watchdog*** de "sin frames con contenido cambiando" (PoC-2, pendientes 4 y 6).
6. **Rutas de error del codificador** (recreación tras 3 errores, fatal con 9): implementadas pero no provocadas en las pruebas.
