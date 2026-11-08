# 9. Contrato de sesión del MVP (F2)

Contrato entre el **emisor** (fuente: monitor virtual → captura → codificación) y el **receptor** (visor: decodificación → presentación). Lo implementan dos agentes en paralelo y debe bastar para que ambos lados interoperen sin coordinarse.

## Roles y conexión
- **El receptor escucha y la fuente conecta.**
  - Receptor: `crossscreen-viewer --listen 0.0.0.0:47810 [--data-dir D] [--fullscreen] [--monitor N]`.
  - Fuente: `crossscreen stream <ip:47810> --peer <huella|prefijo> [--data-dir D] [--mode extend|duplicate] [--width 1920 --height 1080 --hz 60] [--mbps 20] [--fec 0.1] [--pacing 0.6] [--seconds S]`.
- **Seguridad:** QUIC con TLS 1.3 mutuo y huellas fijadas (`cs_transport::Endpoint`, ALPN `cs_protocol::ALPN`).
  - El receptor acepta solo pares de su `TrustStore` (`PeerPolicy::pinned`).
  - La fuente conecta con `PeerPolicy::Exactly(huella)`.
  - Ambos usan `Identity::load_or_create(data_dir)` y `TrustStore::open(data_dir/trusted-peers.json)`. El emparejamiento previo se hace con `crossscreen pair-listen` / `pair` usando los mismos `--data-dir`.
- **Una sesión por conexión y un flujo (`stream_id = 1`).**

## Canal de control
**Transporte:** un stream bidireccional abierto por la fuente, con `cs_transport::FramedStream` y mensajes `cs_protocol::control::ControlMessage`.

**Secuencia:**
1. Fuente → `Hello { version: PROTOCOL_VERSION, device_id, device_name, roles }`.
2. Receptor → `Hello { … }`. Si la versión mayor difiere, cualquiera de los dos envía `Bye { IncompatibleVersion }` y cierra.
3. Receptor → `SinkCapabilities { decoders: [H264, hardware], native_mode, dpi }`.
   - `native_mode` es la resolución del monitor donde se presentará: a pantalla completa, la del monitor; en ventana, la que pida `--size` o 1920×1080.
4. Fuente → `StartStream { stream_id: 1, mode, codec: H264, display_mode, bitrate_kbps }`.
   - `display_mode` es el modo aplicado al monitor virtual (por defecto `native_mode` del receptor, si el driver lo admite; si no, el pedido por CLI).
5. Receptor → `StreamStarted { stream_id: 1 }` cuando el decodificador y la ventana están listos.
6. Fuente: tras `StreamStarted`, el primer frame enviado **debe ser IDR**.

**Durante la sesión:**

| Mensaje | Sentido | Cuándo | Respuesta esperada |
|---|---|---|---|
| `RequestKeyframe { stream_id }` | Receptor → fuente | Al recibir `ReassemblyEvent::Lost`, si el decodificador falla, o si el primer frame no fue IDR | La fuente fuerza un IDR en el siguiente frame (`EncodeControl.force_idr`). Coalescer: como máximo uno cada 100 ms |
| `ReceiverStats` | Receptor → fuente | Cada ~1 s | Informativo; la fuente lo registra |
| `CursorShape` | Fuente → receptor | Cuando cambia la forma | El receptor actualiza la textura del cursor |
| `CursorPosition` | Fuente → receptor | En cada cambio, como máximo a la frecuencia de refresco | El receptor dibuja el cursor encima del vídeo |
| `Bye` | Cualquiera | Al terminar | Cerrar |

**Coordenadas del cursor:** en píxeles del monitor transmitido; el receptor las escala si presenta a otro tamaño. La forma viene de la captura (`cs-capture`, datos del puntero de DXGI), convertida a RGBA y recortada a 128×128 como máximo.

## Vídeo
- **Datagramas:** `cs_transport::Packetizer::new(1, PacketizerConfig { max_datagram: conn.max_datagram_size(), fec_ratio })` y envío con el *pacer* de `cs_transport::pacer` (por defecto *spread* 0,6).
- **Metadatos de cada frame (`FrameMeta`):**
  - `frame_seq`: consecutivo desde 0.
  - `keyframe`: si es IDR.
  - **`pts_us`: instante de captura en microsegundos de `QueryPerformanceCounter`, módulo 2³².** Se usa el `LastPresentTime` de DXGI si está disponible, o el QPC al adquirir el frame. Con fuente y receptor en el mismo equipo (mismo reloj QPC), el receptor puede calcular la **latencia captura → presentación** real. En equipos distintos ese valor no es comparable y no debe mostrarse como latencia.
- **Contenido:** una unidad de acceso H.264 Annex-B por frame, tal como sale de `cs-encode`, con SPS/PPS en cada IDR.
- **Recepción:** `cs_transport::Reassembler::new(1)`.
  - `Frame` → decodificar → presentar.
  - `Lost` → `RequestKeyframe`, y mantener en pantalla el último frame bueno hasta el siguiente IDR (no decodificar P-frames huérfanos).

## Aclaraciones (revisión tras la primera prueba de extremo a extremo, 2026-11-08)
- **Cursor:** `CursorPosition` es la posición del **punto activo** (*hotspot*), no la esquina de la imagen. El receptor dibuja la imagen en `pos − hotspot`, escalado al tamaño presentado.
- **Frames perdidos por completo:** si no llega ningún datagrama de un frame, `Reassembler` emite ahora `Lost` para los huecos de `frame_seq` en cuanto se completa un frame posterior (hasta 256 por hueco). Los receptores no necesitan detectarlo por su cuenta.
- **`ReceiverStats`:** cada ~1 s (prevalece sobre el comentario del código, que decía ~100 ms).
- **"Presentación":** el instante en que retorna `Present()`. El tiempo hasta el *vblank* se informa aparte; los fotones llegan después.
- **Cambio de modo durante la sesión:** no hay mensaje dedicado. La fuente recrea el codificador y envía un IDR con SPS nuevo, y el receptor se reconfigura a partir del SPS. Aún no se ha probado, porque `cs-display` no cambia el modo de un monitor ya activo.
- **Peticiones de IDR redundantes:** la fuente puede ignorar un `RequestKeyframe` que llegue menos de un RTT después de haber forzado un IDR.

## Fin de sesión
- **La fuente**, al terminar (Ctrl+C, `--seconds`, o conexión perdida durante más de 10 s), retira el monitor virtual (`cs_display::Displays::hide`). **Siempre**, también si hay error.
- **El receptor** cierra la ventana y vuelve a escuchar, o sale si se pasó `--once`.

## Métricas que debe imprimir cada lado al terminar
- **Fuente:** fps capturados, codificados y enviados; latencia de captura y de codificación (p50/p95); bitrate real; IDR forzados.
- **Receptor:** fps presentados; frames perdidos; latencia de decodificación y de presentación (p50/p95); y, solo en el mismo equipo, latencia captura → presentación (p50/p95/p99).
