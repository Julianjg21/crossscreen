# ADR-0012: HEVC y AV1 por hardware, opcionales y negociados

- **Estado:** Aceptada (2026-11-09)
- **Contexto:** F8 (avanzado).

## Contexto

H.264 es el códec validado en todas las combinaciones, pero a igual tasa de bits HEVC y AV1 dan más calidad, o la misma calidad con menos ancho de banda (útil en Wi-Fi). El protocolo ya preveía el códec en `StartStream` y la lista de decodificadores del receptor en `SinkCapabilities`; solo el código asumía H.264.

En Windows, Media Foundation ofrece codificadores por hardware del fabricante de la GPU (Intel, NVIDIA, AMD). Los decodificadores de HEVC y AV1 los instalan las extensiones de vídeo de la Microsoft Store («Extensiones de vídeo HEVC» y «Extensión de vídeo AV1»), con aceleración DXVA sobre D3D11.

## Decisión

1. **Por defecto, H.264.**
   - **Opciones:** el usuario elige «H.264» (por defecto), «Automático» (AV1, si no HEVC, si no H.264), «HEVC» o «AV1».
   - **Interfaz:** panel, CLI `--codec` e IPC 1.8.
2. **Negociación:** el emisor usa un códec solo si este equipo lo **codifica** por hardware y el receptor lo **decodifica**. Si se pidió uno concreto y no se puede, vuelve a H.264 y dice por qué. La elección es una función pura con tests (`cs-pipeline/src/codec.rs`) y la reconexión mantiene el códec acordado.
3. **Detección real, no por registro:** un MFT registrado no garantiza que funcione. El driver de Intel de 12.ª generación registra un codificador AV1 que el hardware no tiene, y falla al empezar el *streaming*. Por eso:
   - el emisor crea de verdad un codificador pequeño de cada códec (`hw_encoder_codecs`);
   - el receptor crea cada decodificador con D3D11 (`supported_codecs`) y anuncia solo los que funcionan. El sondeo va en un hilo propio, porque COM multihilo en el hilo de la ventana impediría a winit inicializar OLE.
4. **Flujo de bits** (`cs-encode/src/bitstream.rs`):
   - **H.264 y HEVC:** Annex-B. Se detecta el fotograma clave (IDR en H.264; IDR/CRA en HEVC) y los parámetros de secuencia (SPS+PPS; VPS+SPS+PPS) para anteponerlos a un fotograma clave que no los traiga.
   - **AV1:** OBU con tamaño. La cabecera de secuencia hace de parámetros y se inserta tras el delimitador temporal. Si el tipo de salida trae `av1C`, se toman sus OBU.
5. **Linux** sigue en H.264 (OpenH264) como emisor y como receptor. Con «Automático», un receptor Linux recibe H.264 sin avisos; si se pide HEVC o AV1, se avisa.

## Consecuencias

- **A favor:**
  - mejor calidad por bit cuando los dos equipos lo permiten;
  - ningún cambio de protocolo;
  - el camino validado (H.264) sigue siendo el de por defecto.
- **En contra:**
  - HEVC y AV1 solo entre Windows;
  - el receptor necesita las extensiones de vídeo de la Store;
  - el códec se decide al empezar (no cambia a mitad de sesión).
- **Pruebas en este PC** (Intel i5-1235U, Iris Xe):
  - **Detección:** codifica H.264 y HEVC; decodifica H.264, HEVC y AV1.
  - **Ida y vuelta por hardware:** 60 de 60 frames en H.264 y en HEVC, con error de luma 0,8/255.
  - **HEVC de punta a punta (Windows → Windows, 1080p):** ~65 fps, 520 frames, 0 pérdidas y 0 errores de decodificación.
  - **Elección:** «Automático» elige HEVC, y pedir AV1 vuelve a H.264 con aviso.
  - **AV1 recibido:** un flujo AV1 real (SVT-AV1 en baja latencia, generado con ffmpeg solo para la prueba) se decodifica por hardware: 30 de 30 frames con la luma exacta.
  - **Pendiente:** la codificación AV1 no se ha probado; necesita una GPU con AV1 (Intel Arc / Core Ultra, NVIDIA RTX 40, AMD RX 7000).
- **Siguiente:**
  - HDR (HEVC Main10 / AV1 10 bits);
  - HEVC/AV1 en Linux con VAAPI.
