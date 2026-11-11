# ADR-0005: Codificación con APIs nativas y SDK de fabricante; FFmpeg solo LGPL y dinámico

**Estado:** Propuesta (validar con la PoC-3) · **Fecha:** 2026-11-08

## Contexto
- Weylus incluye un FFmpeg compilado con `--enable-gpl` y x264.
- Sunshine usa FFmpeg para todo salvo NVENC en Windows.
- ScreenExtend carga los SDK de los fabricantes con `libloading` (sin FFmpeg) e incluye una DLL de x264, que es GPL.

Nuestra licencia (ADR-0001) excluye todo lo GPL.

## Decisión
- **Windows:**
  - Media Foundation HW MFT como backend universal del MVP (entrada D3D11 *zero-copy*);
  - después, NVENC, AMF y oneVPL **directos**, cargados en tiempo de ejecución con `libloading` (sin dependencia de enlace) y seleccionados por sondeo de capacidades.
- **Linux:**
  - VAAPI, mediante FFmpeg **compilado como LGPL y enlazado dinámicamente** o mediante `cros-libva` (se decide en la PoC-3 por madurez y soporte de DMA-BUF);
  - después, NVENC.
- **Fallback por software:** OpenH264 con el binario oficial de Cisco descargado en tiempo de ejecución (cubre las regalías de patente H.264).
- **Configuración de baja latencia común:**
  - sin frames B;
  - CBR con VBV de ~1 frame;
  - *intra refresh* en lugar de IDR periódicos;
  - IDR bajo demanda;
  - invalidación de referencias donde exista.

## Alternativas
- **FFmpeg GPL con x264.** Incompatible con la licencia.
- **GStreamer.** Añade una capa grande; sus *plugins* tienen licencias mixtas; complica la ruta *zero-copy* con D3D11 en Windows.

## Revisión tras la PoC-3 (2026-11-08)
Ver [poc-03-codificacion.md](../poc-results/poc-03-codificacion.md). Probado en una Intel Iris Xe (Quick Sync vía MF HW MFT).
- **Funciona** con entrada D3D11 y conversión BGRA→NV12 en la GPU (≤ 1 ms p95). Los IDR bajo demanda salen en el mismo frame.
- **No cumple el objetivo de latencia en este equipo:** a 1080p60 el p95 ronda los 11 ms (objetivo ≤ 6 ms), y en 4K se queda en unos 40 fps. Los SDK directos (oneVPL/QSV, NVENC, AMF) pasan a ser prioritarios para bajar la latencia.
- **Requisitos de proceso, sin los cuales la latencia sube a ~30 ms y 31 fps:**
  - `timeBeginPeriod(1)`;
  - excluir el proceso de la limitación de energía (`ProcessPowerThrottling`);
  - modo de baja latencia del códec.

  Afecta en especial al agente, que corre sin ventana.
- **El cambio de bitrate en caliente no funciona en el MFT de Intel:** `ICodecAPI` devuelve S_OK pero lo ignora. El control de tasa adaptativo (§2.8) tendrá que reconfigurar el codificador en un IDR o usar el SDK directo.

## Revisión: VA-API en Linux (2026-11-11)
Motivo: con OpenH264 por CPU, el emisor Linux se retrasaba y congelaba con el equipo cargado (portátil del dueño: i5-6200U, 2 núcleos / 4 hilos, Intel HD 520).
- **Decisión:** VA-API **directa**, con `libva.so.2` y `libva-drm.so.2` cargadas en tiempo de ejecución (`libloading`, como OpenH264), en `cs-encode/src/vaapi.rs`.
  - No hay dependencia de enlace: sin libva o sin GPU compatible, el emisor sigue con OpenH264.
  - Se descartaron `cros-libva`/`cros-codecs` (enlazan libva al compilar: el agente no arrancaría sin ella) y el FFmpeg de Ubuntu (compilado como GPL).
- **Flujo:**
  - perfil High, CABAC y 8×8;
  - una referencia y sin B;
  - `pic_order_cnt_type` 2 y VUI con `max_num_reorder_frames = 0`;
  - cabeceras SPS/PPS/*slice* escritas por nosotros (`cs-encode/src/h264_writer.rs`, validadas con el analizador `h264.rs`).
- **Punto de entrada:** `EncSliceLP` (VDEnc) si existe; si no, `EncSlice`.
- **Control de tasa:** el driver iHD en Skylake solo ofrece `VA_RC_CQP` para VDEnc, así que hay un control de tasa propio por QP y un objetivo que el regulador de red (`cs-pipeline/src/governor.rs`) puede bajar en caliente.
- **Estructuras C:** comprobadas contra las cabeceras de libva 2.20 (tamaños y `offsetof` en un test).
- **Medido en el i5-6200U a 1366×768:**
  - codificación p50 3,1 ms / p95 3,8 ms, frente a 9,8 / 13,3 ms con OpenH264;
  - CPU por frame 1,6 ms frente a 10,3 ms;
  - con la CPU saturada, máximo 29 ms frente a 242 ms;
  - 120/120 frames decodificados por OpenH264 sin retención.
- **Sin conversión de color por GPU:** en esta GPU el driver no ofrece `VideoProc`, así que RGB→YUV sigue en CPU, con un núcleo optimizado e hilos fijos de prioridad alta.
- **Pendiente:** DMA-BUF sin copias cuando el driver tenga `VideoProc`; NVENC.

## Consecuencias
- Hay que mantener varios backends.
- Ganamos control fino de la latencia y ninguna dependencia GPL.
- Hace falta una matriz de pruebas por fabricante de GPU.
