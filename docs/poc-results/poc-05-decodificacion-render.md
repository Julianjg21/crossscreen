# PoC-5: Decodificación H.264 por hardware y presentación sin copias (Windows)

**Estado:** completada en Windows. Hay decodificación por hardware, presentación **zero-copy** y un p95 de decodificación + presentación de 3,6–5,2 ms en ventana (objetivo: ≤ 6 ms). En pantalla completa salió 6,2 ms en una ejecución corta de 4 s, ligeramente por encima.
**Pendiente:** Linux (VAAPI + DMA-BUF), la ruta `wgpu` y un equipo con GPU dedicada o híbrida.
**Fecha:** 2026-11-08.

**Entorno:**
- Windows 11 Pro 10.0.26200, portátil ASUS con una sola GPU (Intel Iris Xe, LUID `0xFC4D`) y panel 1920×1080 a 60 Hz.
- Rust estable, `windows` 0.58 y `winit` 0.30. Sin ffmpeg ni dependencias GPL/LGPL.

**Código:**
- `crates/cs-decode`: `VideoDecoder` (Media Foundation + D3D11), separador Annex-B (`annexb`), percentiles (`stats`) y un codificador de prueba (`test_encoder`).
- `crates/cs-render`: `Renderer` (D3D11 + DXGI *flip model*) y cursor superpuesto.
- `apps/viewer` (`crossscreen-viewer`): reproduce un fichero Annex-B y mide los tiempos.

## Resultados

| Pregunta | Resultado | Evidencia |
|---|---|---|
| ¿Decodifica el MFT de Windows por hardware sobre D3D11? | ✅ `CLSID_CMSH264DecoderMFT` acepta `MFT_MESSAGE_SET_D3D_MANAGER` y entrega superficies DXGI (`IMFDXGIBuffer`) | El motor **VideoDecode** de la GPU trabaja al 14–16 % para el PID del visor (contador `\GPU Engine(*engtype_VideoDecode)`) con 1080p60 |
| ¿Sale un frame por cada AU, sin retención? | ✅ Con `MF_LOW_LATENCY` y `CODECAPI_AVLowLatencyMode`, cada AU produce su frame en la misma llamada (475 AUs → 475 frames) | Test `roundtrip` y el resumen del visor |
| ¿Funcionan SPS/PPS en línea y `MF_E_TRANSFORM_STREAM_CHANGE`? | ✅ El primer IDR provoca un *stream change* que se resuelve al renegociar NV12. El tamaño visible sale de `MF_MT_MINIMUM_DISPLAY_APERTURE` (1080 visibles de una superficie de 1088) | `stream changes 1` |
| ¿Puede leer el *shader* la superficie del decodificador? | ✅ Con `MF_SA_D3D11_BINDFLAGS = DECODER \| SHADER_RESOURCE`, el MFT crea un array NV12 de 1920×1088×6 con SRV directas (`R8_UNORM` para la luma, `R8G8_UNORM` para la croma) | Log `zero-copy: SRV directa…` |
| ¿Es zero-copy? | ✅ **Sí: ninguna copia a CPU ni entre texturas.** El decodificador escribe la superficie y el *pixel shader* la lee y convierte NV12→RGB en el *back buffer*. El respaldo (copia GPU→GPU) existe, pero no hizo falta | — |
| ¿Contenido correcto en pantalla? | ✅ Se leyó el *back buffer* y se guardó como PNG: las barras BT.709 tienen los colores correctos, el cuadrado móvil está en su sitio y el cursor se ve. Los ficheros del agente codificador (Quick Sync, perfil High) también se decodifican sin artefactos | Capturas en el scratchpad (ver abajo) |
| ¿Ventana sin bordes a pantalla completa? | ✅ `--fullscreen` (borderless de winit sobre flip model), 58,2 fps | Ver tabla de tiempos |
| ¿Decodificación + presentación ≤ 6 ms p95 a 1080p60? | ✅ en ventana: **3,6 ms p95** con contenido sintético y 4,3 ms p95 a 1080p y 40 Mb/s (Quick Sync). ⚠️ En pantalla completa, 6,2 ms p95 (render GPU p95 de 3,8 ms frente a 1,7 en ventana; hay que repetirlo con más muestras) | Ver tabla |

## Decisión: D3D11 + DXGI flip en Windows (no `wgpu` por ahora)

Las opciones eran: (a) `wgpu` sobre DX12, importando la textura D3D11 con *shared handle* → `ID3D12Device::OpenSharedHandle` → `texture_from_raw` de wgpu-hal; (b) presentar con D3D11 sobre el **mismo dispositivo** que el decodificador; (c) copiar a CPU.

**Se eligió la (b)** porque es la única que da zero-copy de forma fiable hoy:
- Las superficies del decodificador son un **array NV12** con `BIND_DECODER`. Compartirlas con D3D12 exige `MISC_SHARED` (el MFT no lo pone en su *pool*) o una copia intermedia. Además, el soporte de NV12 multiplanar en `wgpu` es experimental y depende del backend.
- Con D3D11 en el mismo dispositivo basta con dos SRV sobre la rebanada del array. Así no hay sincronización entre APIs ni *keyed mutex*.
- La ventana sigue siendo de `winit` (D1 de la arquitectura), y `cs-render` solo recibe el `HWND`. `wgpu` queda como **ruta multiplataforma**: en Linux, VAAPI → DMA-BUF → Vulkan (importación externa de memoria). Allí la API de importación de wgpu-hal sí encaja.

**Cadena de intercambio:** `DXGI_SWAP_EFFECT_FLIP_DISCARD` con 2 búferes BGRA8 y `FRAME_LATENCY_WAITABLE_OBJECT` + `SetMaximumFrameLatency(1)`. El bucle espera el objeto antes de dibujar, así que nunca hay más de un frame en cola. `ALLOW_TEARING` está disponible (`--no-vsync`); por defecto se usa `Present(1)`.

**Conversión de color:** BT.709 de rango limitado, fija en el *shader*. Todavía no se leen el VUI ni `MF_MT_VIDEO_NOMINAL_RANGE`, y el codificador de `cs-encode` debe usar lo mismo.

**Cursor:** `Renderer::set_cursor(pos, visible)` toma la posición en píxeles del vídeo remoto y dibuja una flecha procedimental (un *quad* con *alpha blend*) encima del vídeo. Es independiente del frame, como pide §2.4: el cursor puede moverse a la frecuencia del ratón sin un frame nuevo. En la PoC se mueve en círculo para demostrarlo.

## Números

Binario `--release`, ventana de 1280×720 (salvo que se indique otra cosa), `--fps 60`. Cada fila es una ejecución de 4–8 s. Los tiempos GPU se miden con una *event query* de D3D11 tras decodificar y tras dibujar. Ese modo serializa CPU y GPU y se puede desactivar con `--no-gpu-wait`.

| Flujo | Frames | fps | Decode AU→MFT (CPU) p50/p95 | Decode AU→GPU lista p50/p95 | Render NV12→RGB+cursor (GPU) p50/p95 | `Present()` CPU p50/p95 | **AU→Present sin la espera de vsync** p50/p95 | `Present`→vblank p50 |
|---|---|---|---|---|---|---|---|---|
| Sintético 1080p60 (MFT de software, ~0,3 Mb/s), ventana | 475 | 59,4 | 0,57 / 0,98 ms | 0,81 / 1,40 ms | 1,33 / 1,73 ms | 0,32 / 0,52 ms | **2,56 / 3,59 ms** | 15,2 ms |
| Sintético 1080p60, pantalla completa | 233 | 58,2 | 0,66 / 1,01 ms | 0,92 / 1,30 ms | 2,10 / 3,81 ms | 0,38 / 0,62 ms | **3,49 / 6,20 ms** | 14,5 ms |
| Quick Sync 1080p, 40 Mb/s | 227 | 56,7 | 1,01 / 1,38 ms | 1,28 / 1,73 ms | 1,38 / 1,68 ms | 0,47 / 0,76 ms | **3,28 / 4,31 ms** | 15,1 ms |
| Quick Sync 1440p, 40 Mb/s | 232 | 57,7 | 0,89 / 1,41 ms | 1,17 / 1,86 ms | 1,41 / 1,73 ms | 0,47 / 0,63 ms | **3,18 / 4,12 ms** | 15,1 ms |
| Quick Sync 2160p, 60 Mb/s | 221 | 55,2 | 0,89 / 1,30 ms | 1,17 / 2,03 ms | 1,81 / 3,08 ms | 0,47 / 0,59 ms | **3,61 / 5,20 ms** | 14,7 ms |

Cómo leer la tabla:
- **AU→Present sin la espera de vsync** va desde que se entrega la AU hasta que `Present` devuelve el control, descontando la espera del objeto *waitable*. Esa espera es el ritmo del panel, no trabajo. Es la cifra comparable con el presupuesto de §2.4 (decodificar ≤ 4 ms + presentar).
- **`Present`→vblank** sale de `GetFrameStatistics`: el QPC del *vblank* en que se mostró el frame N menos el QPC de la llamada a `Present` del frame N. Está en torno a un periodo de refresco (~15 ms), lo que corresponde a 1 frame de cola con *max latency* 1: ni más, ni menos.
- La espera del objeto *waitable* fue de ~12–14 ms p50 en ventana. El bucle va adelantado y se para a esperar al panel.
- **Primer frame:** de 66 a 270 ms según la resolución. Es la inicialización del decodificador: el *stream change*, el *pool* de superficies y la compilación de *shaders* en el driver. Pasa solo al arrancar el visor o al cambiar de resolución, y entra en el objetivo de la PoC-11 (relanzar el visor en < 1 s).
- Los p99 de 13–33 ms de los ficheros Quick Sync coinciden con la vuelta al inicio del bucle (2 s de vídeo con IDR al principio) y con algún *tick* tardío del planificador (3–4 por ejecución).

**Copias a CPU durante la reproducción: 0.** La única lectura a CPU es la captura PNG opcional (`--screenshot`).

## Problemas encontrados

1. **El *stall* de 5 s fue un caso aislado.** En la primera ejecución, lanzada a la vez que `Get-Counter`, un frame tardó 5 s en `render` + espera GPU. No se reprodujo en las 6 ejecuciones siguientes y la causa no está confirmada; puede ser la caché de *shaders* del driver en la primera ejecución. Se añadió un aviso en el log para cualquier frame de más de 50 ms. Solo apareció el frame 0.
2. **Cobertura de la *event query*.** No está verificado que la *event query* del contexto inmediato espere al motor de vídeo, que el driver puede ejecutar en otra cola. Por eso "decode AU→GPU lista" (~1 ms a 4K) puede ser una cota inferior. La cifra total sigue siendo válida: el *shader* que lee la superficie se ordena después de la decodificación, así que el render GPU incluye la espera si la hubiera.
3. **`windows` 0.58 no define `CLSID_CMSH264DecoderMFT`.** Se declaró a mano (`{62CE7E72-4C71-4D20-B15D-452831A87D9D}`).
4. **Apartamento COM.** winit inicializa OLE (STA) en el hilo principal, y `CoInitializeEx(MTA)` devuelve `RPC_E_CHANGED_MODE`. Se ignora a propósito: el MFT síncrono funciona en STA.
5. **El contenido sintético del MFT de software comprime demasiado** (~0,3 Mb/s pedidos 8). Por eso se probaron también los ficheros Quick Sync de 10–60 Mb/s del agente codificador, que estaban en `scratchpad/smoke/` y no en `scratchpad/encoded/`.

## Cómo reproducir

```text
# Generar un flujo de prueba (MFT H.264 de software de Windows)
cargo run --release -p cs-decode --example gen_h264 -- synth.h264 1920 1080 60 600
# Reproducir 8 s en ventana y capturar el frame 120
crossscreen-viewer --file synth.h264 --fps 60 --seconds 8 --screenshot shot.png --screenshot-frame 120
# Pantalla completa sin bordes en el monitor 0 (índice de winit, se listan al arrancar)
crossscreen-viewer --file synth.h264 --fullscreen --monitor 0 --seconds 4
```

Tests: el separador Annex-B (AUD, `first_mb_in_slice`, SEI/SPS/PPS, códigos de 3 y 4 bytes) y un *roundtrip* (codificar 45 frames de 640×360 → separar → decodificar por hardware, comprobando el número de frames, el tamaño, los pts y la textura NV12).

## Pendiente
1. **Linux:** VAAPI con exportación DMA-BUF (`vaExportSurfaceHandle`) e importación en Vulkan desde `wgpu`. Hay que documentar Intel y AMD. Respaldo: interop EGL/GL o una copia acotada.
2. Leer el espacio de color del flujo (VUI / `MF_MT_VIDEO_NOMINAL_RANGE`, BT.709 frente a BT.601, rango completo frente a limitado) en lugar de fijarlo.
3. Probar en un equipo con GPU dedicada y en uno híbrido (riesgo R8). El dispositivo se crea en el adaptador por defecto, y en un híbrido decodificador y ventana podrían caer en adaptadores distintos.
4. Medir *glass-to-glass* real con la PoC-6. Aquí solo se mide hasta el *vblank* en que DXGI informa del frame.
5. Pérdida de paquetes: llamar a `flush` y esperar al IDR siguiente. La API existe, pero no se ha probado con el transporte real.
6. HDR / 10 bits (P010) y HEVC/AV1: no se han probado.
