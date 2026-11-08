# PoC-3: Codificación H.264 por hardware en Windows (Media Foundation + D3D11)

**Estado:** completada para Intel Quick Sync con MF. **1080p60 y 1440p60 funcionan en tiempo real, pero no alcanzan el objetivo de ≤ 6 ms p95 por frame.** 4K60 no llega a 60 fps en este equipo.
**Pendiente:** NVENC/AMF directos, VAAPI en Linux, entrada real de captura, medición de calidad (PSNR/SSIM) y verificar el efecto real de *intra refresh* y de las capas temporales.
**Fecha:** 2026-11-08.

**Entorno:**
- Windows 11 Pro 10.0.26200, portátil ASUS con una sola GPU: Intel Iris Xe (LUID `0xFC4D`).
- El MFT elegido es *Intel® Quick Sync Video H.264 Encoder MFT* (`VEN_8086`). `MFTEnumEx` devuelve **dos** entradas con el mismo nombre, ninguna con `MFT_ENUM_ADAPTER_LUID`, así que se usa la primera.
- Rust 1.99.1, crate `windows` 0.58. Sin FFmpeg ni x264 (ADR-0005).

**Código:**
- `crates/cs-encode`: `annexb.rs` y `h264.rs` son independientes del SO. `mf/mod.rs` y `mf/convert.rs` son el *backend* de Windows.
- Banco: `crates/cs-encode/examples/encode_bench.rs`.

## Método

1. **Dispositivo.** D3D11 en el adaptador Intel con `VIDEO_SUPPORT | BGRA_SUPPORT` y `ID3D11Multithread::SetMultithreadProtected(TRUE)`. El MFT recibe el **mismo** dispositivo mediante `IMFDXGIDeviceManager` y `MFT_MESSAGE_SET_D3D_MANAGER`.
2. **MFT.** Se obtiene con `MFTEnumEx(VIDEO_ENCODER, HARDWARE | SORTANDFILTER, NV12 → H264)`, se activa y se desbloquea con `MF_TRANSFORM_ASYNC_UNLOCK`. Los eventos `METransformNeedInput` / `HaveOutput` / `DrainComplete` se leen en un hilo con `GetEvent` bloqueante y se reenvían por un canal. `IMFShutdown` desbloquea ese hilo al cerrar.
3. **Conversión BGRA→NV12** en la GPU con `ID3D11VideoProcessor`:
   - Espacios de color DXGI: `RGB_FULL_G22_NONE_P709` → `YCBCR_STUDIO_G22_LEFT_P709`.
   - Procesamiento automático desactivado.
   - *Pool* de 8 texturas NV12 con `RENDER_TARGET | VIDEO_ENCODER`.
   - Cada textura se entrega con `MFCreateDXGISurfaceBuffer`, **sin copia a CPU**.
4. **Contenido sintético generado en la GPU que cambia en cada frame.** El resultado es más exigente que un escritorio típico:
   - Fondo de color variable con `ClearRenderTargetView` completo en cada frame.
   - **Cuatro** bloques de 512×512 con textura detallada (degradado, rejilla tipo texto y ruido) que se mueven a velocidades distintas, copiados con `CopySubresourceRegion`.
   - Una franja de ruido nueva de 256×64 por frame con `UpdateSubresource`.
5. **Medición.**
   - Ritmo real de 60 fps con un temporizador de alta resolución y 600 frames (10 s) por configuración. Se fuerza un IDR cada 120 frames.
   - "Latencia" es el tiempo desde que `encode()` recibe la textura BGRA hasta que la salida H.264 está en CPU: incluye la conversión, la espera de `NeedInput`, la codificación y la copia.
   - "Solo MFT" es el tiempo desde `ProcessInput` hasta la salida.
   - Cada flujo se valida con el analizador propio: NAL Annex-B, SPS con VUI, tipo de *slice* y PPS.

Todas las cifras son de `encode_bench` compilado en *release*.

## Resultados

### Conversión BGRA→NV12 (Video Processor)

Se midió en CPU hasta que se completa un `D3D11_QUERY_EVENT` posterior. Las consultas *timestamp* del contexto 3D **no sirven**: en Intel el Video Processor corre en otro motor y daban 0,02 ms a 4K.

| Resolución | p50 | p95 | máx | Ciclo vacío (p50) |
|---|---|---|---|---|
| 1920×1080 | 0,71 ms | 0,91 ms | 1,34 ms | 0,22 ms |
| 2560×1440 | 0,64 ms | 0,95 ms | 2,45 ms | 0,27 ms |
| 3840×2160 | 0,53 ms | 0,81 ms | 1,13 ms | 0,26 ms |

✅ Cumple el presupuesto de §2.4 (≤ 1 ms). El coste real es de unos 0,3–0,5 ms tras restar el ciclo vacío. No depende de la resolución porque lo hace el bloque de vídeo de función fija.

### Codificación con ritmo real de 60 fps (600 frames, CBR, VBV de 1 frame, High, GOP 600)

| Configuración | fps logrados | Latencia p50 / p95 / máx | Solo MFT p50 / p95 | Tasa real | IDR medio | P medio | IDR/P |
|---|---|---|---|---|---|---|---|
| 1080p, 10 Mbps | 60,0 | 12,1 / 14,6 / 19,4 ms | 11,3 / 13,4 ms | 10,93 Mbps | 43,9 KB | 22,6 KB | 1,9× |
| 1080p, 20 Mbps | 60,0 | 9,2 / 11,1 / 31,8 ms | 8,4 / 10,0 ms | 19,96 Mbps | 110 KB | 41,0 KB | 2,7× |
| 1080p, 40 Mbps | 60,0 | 9,2 / 11,0 / 21,3 ms | 8,6 / 10,3 ms | 39,91 Mbps | 200 KB | 82,2 KB | 2,4× |
| 1440p, 20 Mbps | 60,0 | 13,6 / 15,5 / 22,5 ms | 13,1 / 14,8 ms | 20,00 Mbps | 119 KB | 41,0 KB | 2,9× |
| 1440p, 40 Mbps | 60,0 | 13,4 / 15,2 / 23,3 ms | 12,9 / 14,7 ms | 39,89 Mbps | 184 KB | 82,3 KB | 2,2× |
| 2160p, 40 Mbps | **40,2** | 24,5 / 27,4 / 31,0 ms | 22,6 / 25,6 ms | 40,00 Mbps | 343 KB | 81,1 KB | 4,2× |
| 2160p, 60 Mbps | **41,8** | 24,1 / 27,2 / 32,5 ms | 22,0 / 25,5 ms | 60,27 Mbps | 472 KB | 123 KB | 3,8× |

En todas las configuraciones:
- **Cada frame sale en la misma llamada**: 1 entrada → 1 salida, sin salidas tardías.
- Tasa CBR exacta.
- Los 4 IDR pedidos con `CODECAPI_AVEncVideoForceKeyFrame` **salieron como IDR en el mismo frame**: 4/4 en todas las configuraciones, sin latencia adicional (el IDR tarda lo mismo que un P).

### Rendimiento sin ritmo (300 frames, encadenando frames)

| Resolución | fps | Latencia p50 / p95 | Solo MFT p50 / p95 |
|---|---|---|---|
| 1080p, 20 Mbps | 145 | 6,6 / 8,1 ms | 5,0 / 6,3 ms |
| 1440p, 40 Mbps | 101 | 9,8 / 11,6 ms | 8,0 / 10,1 ms |
| 2160p, 40 Mbps | 59 | 16,6 / 18,7 ms | 14,9 / 17,1 ms |

**Lectura.** El codificador es más rápido trabajando en continuo (5 ms a 1080p) que con ritmo de 60 fps (8–11 ms). La explicación más probable es que la GPU baja de frecuencia entre frames con poca carga. No se ha verificado con contadores de frecuencia. Si es así, el pipeline real (captura → codificación, con la GPU ocupada en DWM) podría quedar en algún punto intermedio. Este dato es de la PoC-6.

### Validación del flujo (los 7 ficheros)

- **Annex-B correcto.** Todas las unidades empiezan con `00 00 00 01` y ninguna tiene `forbidden_zero_bit`.
- **Estructura de cada frame:** AUD (9), SEI (6) y PPS (8) en **todos** los frames, más el *slice*. Los IDR llevan además el SPS (7) en banda. **5/5 IDR con SPS + PPS.** No hizo falta anteponerlos, y `MF_MT_MPEG_SEQUENCE_HEADER` **no está** en el tipo de salida (0 B).
- **Sin *slices* B.** Un *slice* por frame. `poc_type = 2`, que implica orden de salida igual al de decodificación.
- **SPS:**
  - `profile_idc = 100` (High), CABAC activo y `max_num_ref_frames = 1`.
  - `level_idc`: 4.2 a 1080p, 5.1 a 1440p y 5.2 a 2160p.
  - Dimensiones correctas: 1080p se codifica como 1088 con recorte.
- **VUI:**
  - Rango limitado y temporización de 120/1 (60 fps).
  - HRD NAL con CBR a la tasa pedida.
  - **`max_num_reorder_frames = 0` y `max_dec_frame_buffering = 1`.** Esto es clave para el decodificador: puede sacar cada frame sin esperar.
  - No incluye `colour_description`, aunque el tipo de medio declara BT.709. El receptor debe asumir BT.709 limitado.

### Propiedades `ICodecAPI` del MFT Intel (entregable clave)

**Fijadas antes de los tipos de medio, como hace el codificador:**

| Propiedad | Valor pedido | Resultado |
|---|---|---|
| `CODECAPI_AVScenarioInfo` | DisplayRemoting (1) | ✅ Aceptada, se lee 1 |
| `CODECAPI_AVLowLatencyMode` | `true` | ✅ Aceptada. `CODECAPI_AVEncCommonLowLatency` pasa a `true` |
| `CODECAPI_AVEncCommonRateControlMode` | CBR (0) | ✅ Aceptada |
| `CODECAPI_AVEncCommonMeanBitRate` | 10/20/40/60 Mbps | ✅ Aceptada; la tasa real coincide |
| `CODECAPI_AVEncCommonBufferSize` | 1 frame (p. ej. 333 kbit a 20 Mbps) | ✅ Aceptada. El HRD del SPS muestra `cpb_size` = 2,66 Mbit a 20 Mbps; el SPS no refleja el valor pedido |
| `CODECAPI_AVEncMPVGOPSize` | 600 | ✅ Aceptada; solo hay IDR periódico cada 600 frames |
| `CODECAPI_AVEncMPVDefaultBPictureCount` | 0 | ✅ Aceptada, 0 *slices* B |
| `CODECAPI_AVEncMPVProfile` | High (100) | ✅ `SetValue` correcto, pero `GetValue` devuelve VT_EMPTY. El perfil efectivo es High por `MF_MT_MPEG2_PROFILE` |
| `CODECAPI_AVEncH264CABACEnable` | `true` | ✅ Aceptada (CABAC en el PPS) |
| `CODECAPI_AVEncVideoTemporalLayerCount` | 2 | ✅ Aceptada (rango 1..3). **No se verificó la estructura de capas** en el flujo |
| `CODECAPI_AVEncVideoGradualIntraRefresh` | 1 (VT_UI4) | ✅ Aceptada y se lee 1. **No se verificó** que haga *intra refresh* real; se siguen viendo IDR normales |
| `CODECAPI_AVEncVideoGradualIntraRefresh` | 1 (VT_UI8) | ❌ `E_INVALIDARG`: el tipo es VT_UI4 |
| `CODECAPI_AVEncVideoLTRBufferControl` | `0x10002` (2 LTR) | ✅ Aceptada, se lee `0x10002`. El valor por defecto es `0x10000` |
| `CODECAPI_AVEncVideoMaxNumRefFrame` | 1 | ✅ Aceptada (rango 0..16) |
| `CODECAPI_AVEncSliceControlMode` | 2 (filas de MB) | ✅ Aceptada. Es el único modo del rango [2..2] |
| `CODECAPI_AVEncSliceControlSize` | 17 filas | ❌ `SetValue` correcto, pero **`NOTIFY_BEGIN_STREAMING` falla con `E_FAIL`**. Fijado durante el streaming devuelve `E_INVALIDARG` |
| `CODECAPI_AVEncCommonQualityVsSpeed` | 0 (velocidad) | ✅ Aceptada (por defecto 50). Sin efecto medible en la latencia a 1080p20: 8,3 ms p50 |
| `CODECAPI_AVEncCommonRateControlMode` | LowDelayVBR (4) | ❌ `E_INVALIDARG` |
| `CODECAPI_AVEncVideoMinQP` / `MaxQP` | — | ❌ No soportadas (`IsSupported` = false) |
| `CODECAPI_AVEncNumWorkerThreads`, `AdaptiveMode`, `CommonRealTime`, `H264SPSID` | — | ❌ No soportadas |
| `CODECAPI_AVEncCommonMaxBitRate` | — | Soportada, pero `GetValue` devuelve `E_NOTIMPL` y no es modificable |
| `CODECAPI_AVEncVideoEncodeFrameTypeQP` | — | Soportada, pero `GetValue` devuelve `E_NOTIMPL` y no es modificable |

**Durante el streaming:**

| Propiedad | Resultado |
|---|---|
| `CODECAPI_AVEncVideoForceKeyFrame = 1` | ✅ Funciona: IDR en el mismo frame, 4/4 en cada pasada |
| `CODECAPI_AVEncCommonMeanBitRate` 20 → 5 Mbps | ⚠️ **`SetValue` devuelve S_OK, pero el MFT lo ignora.** `GetValue` sigue leyendo 20 Mbps y la tasa real se mantuvo (20,0 → 19,9 Mbps) |
| Resto (fijadas tras `BEGIN_STREAMING`) | ⚠️ `SetValue` devuelve S_OK, pero `GetValue` no cambia y `IsModifiable` da `false`. **Hay que fijarlas antes de los tipos.** Por eso existe `MfH264Encoder::new_with_properties` |

## Hallazgos

1. **Resolución del temporizador: es el hallazgo más importante.**
   - Con la resolución por defecto de Windows (15,6 ms), el MFT asíncrono de Intel avanza a saltos de un *tick*. En 1080p20 se midieron 15,9 ms "solo MFT" más ~15 ms de espera de `NeedInput`, lo que da **~30 ms de latencia y solo 31 fps**, igual a 1080p y a 4K.
   - Con `timeBeginPeriod(1)` baja a **9,4 ms y 60 fps**.
   - Windows 11 puede ignorar esa petición en procesos sin ventana visible. En una pasada completa sin la exclusión, 1440p volvió a 31 ms. Por eso el codificador hace dos cosas:
     - Llama a `timeBeginPeriod(1)`, que se deshace en `Drop`.
     - Pide `SetProcessInformation(ProcessPowerThrottling)` con `IGNORE_TIMER_RESOLUTION | EXECUTION_SPEED` desactivados.
   - Con ambas cosas, todas las pasadas fueron estables. Está en `EncoderConfig::raise_timer_resolution`, activado por defecto.
   - **Implicación para `crossscreen-agent`:** el agente corre sin ventana. Tiene que hacer lo mismo o perderá unos 20 ms.
2. **`CODECAPI_AVLowLatencyMode` es imprescindible.** Sin él, el MFT retiene frames: con nuestra espera de 100 ms se midieron unos 104 ms por frame y menos de 10 fps. Con él, 1 entrada → 1 salida.
3. **`DisplayRemoting` y el VBV de 1 frame no cambian la latencia de forma medible.** En las variantes a 1080p20, con temporizador de 1 ms, todas quedaron en 9,4–10,9 ms p50.
4. **Objetivo de la PoC-3 no cumplido en Intel Iris Xe con MF:**
   - 1080p60: 11,1 ms p95 frente a ≤ 6 ms.
   - 4K60: no llega a 60 fps (40–42 fps, unos 27 ms p95) frente a ≤ 12 ms.
   - El presupuesto de §2.4 (≤ 5 ms para codificar) solo se acerca encadenando frames (5,0 ms p50 y 6,3 ms p95 a 1080p).
   - **Opciones para 1080p60 en Intel:** QSV directo (oneVPL, MIT), tamaño de *slice* con la API nativa, o aceptar 9–11 ms en el presupuesto.
5. **SPS/PPS en banda.** El MFT de Intel repite el PPS en cada frame y pone el SPS en cada IDR. No hay `MF_MT_MPEG_SEQUENCE_HEADER`. Aun así, el codificador antepone los últimos SPS/PPS conocidos si un IDR saliera sin ellos (`ensure_parameter_sets_on_idr`), y el contador quedó en 0.
6. **Coste del IDR.** Con CBR y VBV de 1 frame, un IDR ocupa 2–4 veces un P, y su tiempo de codificación es igual al de un P. Para la red (PoC-4) hay que contar con ráfagas de unos 100–470 KB por IDR.
7. **Las propiedades fijadas tarde se aceptan sin efecto.** Que `SetValue` devuelva S_OK no prueba nada: hay que leer el valor después y, mejor aún, comprobar el flujo. El cambio de tasa en caliente **no funciona** con `MeanBitRate` en este MFT. Para la adaptación de tasa queda por probar:
   - reconfigurar con `MFT_MESSAGE_NOTIFY_END_STREAMING` y volver a empezar;
   - recrear el codificador con un IDR;
   - QSV directo.
8. **Entorno.**
   - Durante una prueba previa a las cifras de esta página estaba abierto un juego (VALORANT), que usaba el 44 % del motor 3D de la iGPU. Esas cifras previas se descartaron.
   - Las cifras de la tabla se tomaron con el juego cerrado (solo seguía el cliente de Riot), pero con VS Code y otras compilaciones en paralelo. Son un caso realista, no de laboratorio.

## Ficheros generados

Están en `C:/Users/JULIAN/AppData/Local/Temp/claude/9c55f4c2186a89c6/ee884e23-74ce-4658-b7d1-538bd77cf850/scratchpad/encoded/`:
- `bench_1080p.h264`: copia de 1080p a 20 Mbps.
- `bench_1080p_{10,20,40}mbps.h264`
- `bench_1440p_{20,40}mbps.h264`
- `bench_2160p_{40,60}mbps.h264`

Cada uno tiene 600 frames a 60 fps, Annex-B, High, un IDR al inicio y 4 IDR forzados (frames 120, 240, 360 y 480).

**Los cuatro rectángulos en diagonal del contenido son intencionados.** El generador dibuja 4 copias del mismo patrón de 512×512 en posiciones distintas, que arrancan desplazadas en diagonal (`k·300`, `k·170`) y rebotan a velocidades diferentes. El fondo se borra por completo en cada frame. No es un rastro ni un artefacto de codificación.

## Pendiente

1. **NVENC y AMF directos** (SDK de NVIDIA y AMF, ambos con licencia MIT) y comparar con MF en equipos con GPU dedicada. En este portátil solo hay Intel. **QSV directo** con oneVPL para ver si baja de 6 ms a 1080p.
2. **VAAPI en Linux** con `cros-libva` frente a FFmpeg LGPL, como pide la PoC-3 original.
3. **Entrada real.** Integrar con `cs-capture` (DXGI Desktop Duplication) sobre el mismo `ID3D11Device` y medir de punta a punta en la PoC-6. Falta comprobar que la textura de DDA se puede usar como entrada del Video Processor (o copiarla antes).
4. Verificar en el flujo el efecto de `GradualIntraRefresh`, de las capas temporales (`nal_ref_idc` y SEI) y de LTR. Medir la calidad con PSNR/SSIM.
5. Adaptación de tasa en caliente: buscar un mecanismo que funcione en Intel MF (ver hallazgo 7).
6. Comprobar con contadores si la diferencia entre trabajar con ritmo y en continuo se debe a la frecuencia de la GPU.
