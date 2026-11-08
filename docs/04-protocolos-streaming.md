# 4. Comparación de protocolos de streaming

## 4.1 Qué necesitamos del transporte

1. **Vídeo sin bloqueo de cabeza de línea.** Un paquete perdido no debe detener los frames siguientes, así que el vídeo tiene que ir por un canal **no fiable**.
2. **Control y entrada fiables y ordenados**, en canales independientes del vídeo.
3. **Cifrado y autenticación mutua integrados**, con certificados fijados tras el emparejamiento.
4. **Control total del códec y del *pacing*.** Nada de *jitter buffers* ajenos.
5. **Receptor nativo en Rust** en ambos lados (ver [ADR-0003](adr/0003-receptor-nativo.md)), así que la compatibilidad con navegadores no es un requisito del MVP.
6. **Licencia permisiva y madurez en Rust.**
7. **Robustez en Wi-Fi:** pérdidas en ráfaga, *jitter* y cambios de interfaz.

## 4.2 Opciones

| Opción | Usado por | Vídeo no fiable | Canales fiables | Cifrado y autenticación | Control de *pacing* y códec | Estado en Rust | Licencia | Latencia alcanzable |
|---|---|---|---|---|---|---|---|---|
| **WebRTC (libwebrtc)** | TwinDesk, Deskreen (vía Chromium) | ✅ RTP/SRTP | ✅ DataChannel (SCTP) | DTLS-SRTP; autenticación según la señalización | Limitado: el *jitter buffer* y el control de congestión son de Google; hace falta tocar *field trials* | Bindings C++ pesados (compilar libwebrtc) | BSD | 30–80 ms; mucho trabajo para bajar de 30 |
| **WebRTC (webrtc-rs)** | ScreenExtend | ✅ | ✅ | DTLS-SRTP | Sin *pacer*; el *payloader* H.264 descarta SPS/PPS (documentado por ScreenExtend) | Maduro pero con huecos | MIT/Apache | Depende de lo que se implemente encima |
| **WebRTC (str0m, *sans-IO*)** | — | ✅ | ✅ | DTLS-SRTP | Buen control (*sans-IO*), BWE incluido | Bueno | MIT/Apache | Buena |
| **RTP/UDP propio + ENet** (estilo GameStream) | Sunshine | ✅ | ✅ (ENet) | **Hay que diseñarlo** (Sunshine: AES-GCM opcional y emparejamiento débil) | Total | Hay que escribirlo todo | — | La mejor (< 20 ms en LAN) |
| **QUIC: datagramas (RFC 9221) + streams** | — | ✅ DATAGRAM | ✅ streams multiplexados sin HOL entre sí | **TLS 1.3 integrado**, autenticación mutua y verificador personalizado | Total sobre los datagramas; el control de congestión de QUIC es configurable | **quinn**: maduro, GSO/GRO, migración de conexión | MIT/Apache | Muy buena (objetivo < 25 ms) |
| **SRT** | Difusión profesional | ✅ con ARQ dentro de una ventana de latencia | ⚠️ un solo flujo | AES (PSK), sin autenticación de pares | Ventana de latencia fija, normalmente ≥ 4×RTT (20–120 ms) | Bindings a libsrt | MPL-2.0 | Media; pensado para contribución, no para interacción |
| **WebSocket / TCP** | Weylus | ❌ (HOL) | ✅ | TLS | Bajo | Trivial | — | Mala con pérdidas; picos de cientos de ms |
| **Media over QUIC (MoQ)** | — | ✅ | ✅ | TLS 1.3 | Bueno | Borradores IETF inestables | — | Buena, pero inmaduro |
| **RDP (gráficos H.264 AVC420/444)** | Windows | ✅ (UDP multitransport) | ✅ | TLS / CredSSP | Medio | IronRDP (MIT/Apache) | — | Media; el protocolo es enorme y está orientado a sesión remota |

## 4.3 Decisión: QUIC con quinn (→ [ADR-0002](adr/0002-transporte-quic.md))

**Una conexión QUIC por sesión:**

| Canal | Tipo QUIC | Contenido |
|---|---|---|
| Control | Stream bidireccional 0 (fiable) | Handshake de la aplicación, capacidades, modos, petición de IDR, estadísticas, keep-alive |
| Entrada | Stream unidireccional (fiable, prioridad alta) | Eventos de teclado y ratón. En una fase posterior, movimiento de ratón por datagrama con número de secuencia |
| Cursor | Datagramas (posición) + stream (forma) | Ver §2.4 |
| Vídeo | **Datagramas** | Fragmentos de unidades de acceso H.264 con FEC |
| Audio (futuro) | Datagramas | Opus de 10 ms con FEC dentro de banda |
| Archivos y portapapeles (futuro) | Un stream por transferencia | — |

**Por qué QUIC y no las otras opciones**
- **Frente a WebRTC.** WebRTC resuelve NAT traversal, interoperabilidad con navegadores y señalización. No necesitamos ninguna de las tres en un producto LAN con receptor nativo, y pagaríamos su complejidad: ICE, SDP, DTLS, SCTP y *jitter buffer*. ScreenExtend tuvo que reimplementar el *packetizer* y el *pacer* sobre webrtc-rs. TwinDesk depende de *field trials* de Chromium.
- **Frente a un UDP propio al estilo Sunshine.** Se consigue la misma latencia sin **inventar criptografía**: TLS 1.3 de rustls es auditado, y los streams fiables vienen incluidos (no hace falta ENet). Además, la migración de conexión cubre el paso de Wi-Fi a Ethernet.
- **Frente a SRT.** Su modelo de ventana fija de latencia no encaja con contenido interactivo, y no autentica a los pares.
- **Frente a TCP o WebSocket.** El bloqueo de cabeza de línea es incompatible con el objetivo de latencia.

**Riesgos de QUIC y cómo se validan**
- **Coste de CPU.** Cifrar cada datagrama y la sobrecarga de quinn a 50–80 Mbps podría pesar. Se mide en la PoC-4. *Plan B:* UDP propio con `ring` AES-GCM y claves derivadas del handshake QUIC (exportador TLS), manteniendo QUIC para el control.
- **Interferencia del control de congestión de QUIC** (NewReno/Cubic/BBR) con el control de tasa de la aplicación: los datagramas QUIC **sí** están sujetos a la ventana de congestión. Se mide qué controlador da menos latencia y, si hace falta, se implementa un controlador propio a través del *trait* `Controller` de quinn.
- **Navegador como receptor (futuro).** WebTransport (HTTP/3) podría reutilizar el mismo diseño de datagramas; str0m queda como plan B si se necesitara WebRTC.

## 4.4 Protocolo de vídeo sobre datagramas

**Cabecera de fragmento** (binaria, *little-endian*, 24 bytes; implementada en `crates/cs-protocol/src/video.rs`):

```
 0       1       2       3       4       5       6       7
+-------+-------+-------+-------+-------+-------+-------+-------+
|  ver  | flags |   stream_id   |           frame_seq           |
+-------+-------+-------+-------+-------+-------+-------+-------+
|           frame_len           |            pts_us             |
+-------+-------+-------+-------+-------+-------+-------+-------+
| block | blocks| shard | data  | parity|  rsv  |   shard_len   |
+-------+-------+-------+-------+-------+-------+-------+-------+
flags: KEYFRAME | PARITY | DISCARDABLE
```

> Revisión 2026-11-08: el primer borrador (16 bytes) tenía dos problemas. Un `pts` de 16 bits daba la vuelta cada 65 ms, y los campos FEC de `u8` impedían partir en bloques un IDR grande. Ahora el frame se divide en bloques FEC de hasta 64 shards de datos, con hasta 255 bloques por frame.

- **Tamaño:** la longitud del datagrama se ajusta a `max_datagram_size()` de la conexión QUIC (≈ 1150–1400 B según el MTU de la ruta), lo que evita la fragmentación IP.
- **FEC por bloque:** Reed-Solomon sistemático (crate `reed-solomon-simd`, MIT). Cada bloque tiene `data` shards de datos y `parity` de paridad, todos del mismo tamaño par. El valor de `parity` se adapta a la pérdida medida.
- **Pacing:** los fragmentos de un frame se reparten en ~50–70 % del intervalo entre frames para no saturar los *buffers* del AP Wi-Fi (idea conceptual de Sunshine). Se usa GSO cuando está disponible.
- **Recuperación:**
  - frame irrecuperable → el receptor lo informa por control;
  - la fuente responde con **invalidación de referencias** (NVENC) o **IDR**;
  - el receptor sigue mostrando el último frame bueno. No hay retransmisión de vídeo.
- **Bitstream H.264:**
  - Annex-B, perfil High (Main si el decodificador lo exige) y sin frames B;
  - *slices* por frame para decodificación paralela opcional;
  - SPS/PPS en cada IDR;
  - **intra refresh** periódico en lugar de IDR periódicos, que suaviza los picos de bitrate.
- **Reloj:** `pts_us` es relativo al inicio de la sesión. Para medir la latencia de extremo a extremo con la herramienta `cs-latency`, se sincroniza un reloj estimado (estilo NTP) por el canal de control.

## 4.5 Protocolo de control

- **Formato:** mensajes serializados con **postcard** (serde, compacto, MIT/Apache) en un *frame* de longitud prefijada.
- **Versionado:**
  - `protocol_version` mayor y menor en el handshake;
  - una versión mayor distinta implica rechazo con un mensaje claro;
  - las versiones menores añaden campos opcionales.
- **Negociación de capacidades:**
  - códecs y perfiles soportados por el decodificador del receptor;
  - resolución y frecuencia nativas;
  - HDR;
  - entrada disponible (táctil, pluma).

  La fuente elige la intersección.
- **Mensajes del MVP:**
  - `Hello`, `Capabilities`, `StartStream{mode, codec, bitrate}`, `StreamStarted`;
  - `RequestKeyframe`, `LossReport`, `Stats`;
  - `SetMode`, `CursorShape`, `Bye`.

## 4.6 Códecs

| Códec | Fase | Motivo |
|---|---|---|
| **H.264** | MVP | Codificación y decodificación por hardware universales (todas las GPU desde ~2012, Media Foundation siempre presente, VAAPI). Es el mínimo común denominador entre SO. |
| HEVC | F8 | ~30–40 % menos bitrate a igual calidad. En Windows la decodificación requiere la extensión HEVC; en Linux varía según el driver o la distribución. |
| AV1 | F8 | Excelente para texto y *screen content* (herramientas SCC). Codificación por hardware solo en GPU recientes (RTX 40+, RDNA3+, Arc). |
| H.264 4:4:4 | Futuro | Texto nítido. Pocos decodificadores por hardware lo soportan; es una opción avanzada. |

**Backends de codificación** (→ [ADR-0005](adr/0005-codificacion-sin-ffmpeg-gpl.md)):

| SO | MVP | Siguiente | *Fallback* software |
|---|---|---|---|
| Windows | **Media Foundation HW MFT** (sirve para NVIDIA, AMD e Intel; entrada D3D11 *zero-copy*) | NVENC directo (invalidación de referencias), AMF, oneVPL/QSV, cargados con `libloading` | OpenH264 (binario de Cisco: cubre las licencias de patente H.264) |
| Linux | **VAAPI** (Intel, AMD; NVIDIA con `nvidia-vaapi-driver` solo para decodificar) mediante FFmpeg **compilado como LGPL y enlazado dinámicamente**, o mediante `cros-libva` (a decidir en la PoC-3) | NVENC con CUDA/DMA-BUF | OpenH264 |

**Decodificación:** D3D11VA o MF (Windows) y VAAPI (Linux), entregando texturas que `wgpu` importa. En Linux, la importación DMA-BUF en `wgpu` (Vulkan) **se valida en la PoC-5**; si no fuera viable, se usa EGL y GL interop o una copia acotada.
