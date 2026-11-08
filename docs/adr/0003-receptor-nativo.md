# ADR-0003: Receptor nativo (winit + wgpu) separado de la UI Tauri

**Estado:** Propuesta (validar con las PoC-5 y PoC-11) · **Fecha:** 2026-11-08

## Contexto
La UI se construye con Tauri, React y TypeScript. Tauri usa **WebView2** en Windows y **WebKitGTK** en Linux.

Mostrar vídeo dentro del WebView implica dos problemas:
- **copiar frames decodificados al WebView**, que no puede importar texturas D3D11 ni DMA-BUF;
- **depender de WebCodecs**, que WebKitGTK no garantiza en todas las distribuciones.

Deskreen, Weylus y ScreenExtend usan un navegador como receptor y heredan sus *buffers* (MSE, *jitter buffer* de WebRTC).

## Decisión
- El vídeo se presenta en un **proceso nativo `crossscreen-viewer`**:
  - `winit` para la ventana a pantalla completa en el monitor elegido;
  - `wgpu` para la presentación, importando las texturas del decodificador por hardware;
  - cursor dibujado como capa superpuesta.
- La UI Tauri es solo el panel de control y habla con el agente por IPC.

## Alternativas
- **Vídeo dentro de Tauri con WebCodecs.** Copias adicionales y soporte desigual en Linux. Se descarta.
- **Visor dentro del proceso del agente.** Es más simple, pero un fallo del driver de GPU tumbaría la sesión. Se mantiene como *fallback* si la PoC-11 muestra que la IPC añade complejidad sin beneficio.

## Revisión tras la PoC-5 (2026-11-08)
Ver [poc-05-decodificacion-render.md](../poc-results/poc-05-decodificacion-render.md).
- **En Windows, el visor presenta con D3D11 directo, no con wgpu.** Usa una *swap chain* *flip-model* (`FLIP_DISCARD`, latencia máxima de 1 frame) sobre el **mismo dispositivo** que el decodificador, y un *shader* NV12→RGB que lee la superficie decodificada sin copiarla.
- **Motivo:** el pool de superficies del decodificador MF no se crea compartible, así que pasarlas a D3D12/wgpu exigiría copias; además, NV12 en wgpu es experimental.
- **wgpu sigue siendo la vía prevista para Linux** (VAAPI → DMA-BUF → Vulkan). winit sigue gestionando la ventana en ambos sistemas.
- **Medido:** decodificación + presentación ≈ 3,6 ms p95 en ventana a 1080p60, y 6,2 ms p95 a pantalla completa (una sola pasada corta, por repetir).

## Consecuencias
- Hay que gestionar tres procesos y su IPC.
- La ruta *zero-copy* queda bajo nuestro control.
- La latencia de presentación es predecible (modo mailbox o *immediate*).
