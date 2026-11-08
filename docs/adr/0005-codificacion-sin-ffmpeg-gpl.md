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

## Consecuencias
- Hay que mantener varios backends.
- Ganamos control fino de la latencia y ninguna dependencia GPL.
- Hace falta una matriz de pruebas por fabricante de GPU.
