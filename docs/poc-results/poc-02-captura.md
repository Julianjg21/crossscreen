# PoC-2: Captura del monitor virtual en GPU (Windows, DXGI Desktop Duplication)

**Estado:** funciona. La captura del monitor virtual en GPU y la recuperación tras un cambio de modo están verificadas. El objetivo de ≤ 2 ms p95 se cumple **midiendo solo el coste en CPU** con el sistema en reposo (0,64 ms p95). No se pudo medir en reposo la copia completada en GPU: el equipo estuvo al 100 % de CPU por compilaciones paralelas durante casi todas las tandas.
**Pendiente:** repetir las medidas con el equipo en reposo, probar en una GPU dedicada y en un portátil híbrido, el *fallback* WGC y el escritorio seguro (ver al final).
**Fecha:** 2026-11-08.

**Entorno:**
- Windows 11 Pro 10.0.26200, portátil ASUS con una sola GPU (Intel Iris Xe, LUID `0xfc4d`) y panel 1920×1080 al 125 %.
- Monitor virtual VDD (`MTT1337`) mostrado con `crossscreen display show` (PoC-1), en 1920×1080@60, a la derecha del panel.
- `windows` 0.58, compilación `release`.

**Código:**
- `crates/cs-capture`: API independiente del SO más el backend `windows_dxgi`.
- Bench: `cargo run -p cs-capture --release --example capture_bench -- --target virtual --seconds 10 [--mode-change-at 3] [--no-gpu-wait] [--no-animate] [--save-bmp DIR]`.

## Diseño implementado

| Aspecto | Decisión |
|---|---|
| API | `CaptureSource::next_frame(timeout) -> Result<Option<CapturedFrame<T>>>`. `None` = no hay imagen nueva (escritorio estático o recuperación en curso). El frame incluye la textura, el tamaño, el formato, la secuencia, `AccumulatedFrames`, las marcas QPC (presentación, adquisición, copia lista), las regiones sucias y movidas y `device_generation` |
| Cursor | Fuera de banda: `CursorState` con posición, visibilidad y forma (monocromo, color o color con máscara), más contadores de cambios. Las actualizaciones solo de cursor no generan frame. `CursorShape::to_rgba()` para el receptor |
| Selección de salida | Se enumeran todos los adaptadores y salidas DXGI y se compara `DXGI_OUTPUT_DESC.DeviceName` con el nombre GDI (`\\.\DISPLAY13`), o se elige la principal |
| Dispositivo | `D3D11CreateDevice` sobre **el adaptador que expone la salida**, con `BGRA_SUPPORT` y protección multihilo. Se expone con `DxgiCapture::device()` para que el codificador use el mismo |
| Duplicación | `IDXGIOutput5::DuplicateOutput1` con `[B8G8R8A8_UNORM, R16G16B16A16_FLOAT]` y el hilo en modo DPI por monitor v2. Si devuelve `UNSUPPORTED` se usa `IDXGIOutput1::DuplicateOutput` |
| Copia | `CopyResource` a una textura propia de un *pool* (uso DEFAULT, SRV + RTV) y `ReleaseFrame` inmediato. Una textura se reutiliza solo cuando nadie conserva el frame (`Arc`). Sin lectura a CPU en el camino caliente |
| `ACCESS_LOST` y similares | Se descarta la duplicación y se reintenta en las siguientes llamadas con una fábrica DXGI nueva. Si la salida cambia de adaptador, se recrea el dispositivo (`device_generation`). Margen de recuperación configurable: 5 s por defecto |
| Depuración | `read_to_cpu` (staging + Map) y un escritor BMP propio, sin dependencias, para volcar UN frame |

## Resultados

Condiciones de las tandas:
- Una ventana del bench repinta toda la salida a ritmo de DWM (`DwmFlush`): color de fondo, barra blanca móvil y contador. Así cada frame es distinto.
- "Acquire→copia lista" va desde que vuelve `AcquireNextFrame` hasta que la copia está **encolada** (`--no-gpu-wait`) o **completada en la GPU** (por defecto, con una consulta `D3D11_QUERY_EVENT`).
- La CPU se muestreó con `Win32_PerfFormattedData_PerfOS_Processor` cada 0,7 s.

### Frames reales, no negros

Se volcaron frames a BMP y se revisaron a ojo:
- **Monitor virtual:** se ve la ventana de animación (fondo de color, barra blanca y el texto "CrossScreen PoC-2 frame N"), tanto a 1920×1080 como tras el cambio a 1600×1000.
- **Panel principal:** 309–406 colores distintos en una muestra de 64×64 píxeles. Esta imagen no se abrió a ojo porque es el escritorio del usuario.
- **Defecto en el bench, no en la captura:** un frame del primer segundo mostró restos de la ventana pintada cuando el monitor aún estaba en el modo inicial. Viene de pintar con GDI fuera de `WM_PAINT` mientras Windows aún asienta el modo.

### Latencia y cadencia

| Tanda | Salida | Espera GPU | CPU del sistema | fps entregados | Acquire→copia p50 / p95 / p99 / máx (ms) | DWM→Acquire p50 / p95 (ms) | Frames con `AccumulatedFrames>1` |
|---|---|---|---|---|---|---|---|
| M1 (cambio de modo) | virtual | no | no medida (el animador iba a 57/s) | **53,5** | **0,315 / 0,636 / 1,547 / 7,2** | 0,32 / 0,64 | 4 de 428 |
| B2 | virtual | no | 100 % | 51,5 | 0,292 / 1,091 / 3,237 / 5,0 | 6,1 / 20,8 | 70 de 515 |
| B1 | virtual | no | 100 % | 34,9 | 0,285 / 1,721 / 15,1 / 52,0 | 11,0 / 56,9 | 129 de 349 |
| Primera tanda | virtual | sí | no medida | 46,3 | 3,793 / 16,5 / 59,9 / 116,2 | 4,2 / 17,9 | 34 de 278 |
| A1 | virtual | sí | 100 % | 10,9 | 1,180 / 24,3 / 43,6 / 182,8 | 18,6 / 45,1 | 64 de 109 |
| A2 | virtual | sí | 100 % | 16,5 | 1,484 / 19,6 / 131,4 / 131,4 | 24,8 / 76,2 | 122 de 166 |
| P1 | principal (ventana de 320×240) | sí | 100 % | 47,4 | 1,207 / 3,720 / 10,0 / 10,7 | 5,1 / 32,0 | 136 de 285 |
| P2 | principal (ventana de 320×240) | no | 100 % | 14,8 | 0,244 / 6,384 / 89,5 / 89,5 | 56,2 / 100,6 | 86 de 89 |

**Lectura:**
- **El coste propio de la captura en CPU es de unas 0,3 ms en la mediana en todas las tandas.** Incluye la adquisición ya resuelta, el cursor, los metadatos, encolar la copia y `ReleaseFrame`.
- Con el sistema libre (M1), el p95 es de 0,64 ms y el retraso desde que DWM presenta hasta que lo adquirimos es de 0,3–0,6 ms. **Cumple los ≤ 2 ms p95 de §2.4.**
- Con la CPU al 100 %, las colas se disparan (p99 de 3 a 90 ms) y se pierden frames (`AccumulatedFrames>1`). Es planificación del hilo, no la API: la mediana no cambia. **En producción el hilo de captura necesita prioridad alta** (MMCSS "Capture" o `THREAD_PRIORITY_TIME_CRITICAL`).
- **Esperar a que la GPU termine la copia** cuesta entre 1,2 y 3,8 ms p50 en la Iris Xe. Las únicas medidas con espera son con carga, o con carga desconocida en la primera tanda.
  - Una copia de 1080p BGRA (8 MB) debería tardar bastante menos que eso: la espera incluye la cola de la GPU compartida con DWM y con el pintado GDI de 1080p del propio bench.
  - **En el diseño no hace falta esperar:** el codificador usa el mismo dispositivo y el orden lo garantiza la cola de la GPU. Este coste aparecerá, en todo caso, dentro de la etapa de codificación.
  - **Queda por medir en reposo** la copia en GPU con *timestamps* `D3D11_QUERY_TIMESTAMP`, en lugar de la espera bloqueante.

### Escritorio estático

Prueba sin ventana de animación (tanda C, 6 s):
- Tras asentarse, **0 frames y solo *timeouts*** (42 % de las llamadas en el total de la tanda).
- Hubo 9 actualizaciones solo de cursor, absorbidas en `CursorState` sin generar frames. Es el comportamiento esperado.
- En los primeros 2 s llegaron 42 frames y 2 `ACCESS_LOST`: es el asentamiento posterior a `display show` (ver abajo).

### Cambio de modo en caliente (1920×1080 → 1600×1000)

`crossscreen display show --width 1600 --height 1000` lanzado a mitad de captura:

| Tanda | `ACCESS_LOST` | Reintentos fallidos | Pérdida → duplicación recreada | Pérdida → primer frame | Orden → primer frame 1600×1000 | Duración de la orden |
|---|---|---|---|---|---|---|
| M1 (sistema libre) | 5 (incluye el asentamiento inicial) | 0 | 91 ms | **111 ms** | **243 ms** | 123 ms |
| M0 (carga desconocida) | 2 | 0 | — | 892 ms | 1 068 ms | 209 ms |
| M2 (CPU al 100 %) | 2 | 0 | 613 ms | 1 119 ms | 1 302 ms | 156 ms |
| Anómala | 4 | 0 | se recreó | **nunca, 0 frames en 8 s** | — | 209 ms |

- **La recuperación funciona sin intervención:** `ACCESS_LOST`, una duplicación nueva con el tamaño nuevo y el pool de texturas renovado. No hizo falta recrear el dispositivo porque el adaptador es el mismo. No se vio ningún reintento fallido: `DuplicateOutput1` funcionó al primer intento cada vez.
- **Tanda anómala:**
  - La ventana del bench solo pintó 1 vez en 8 s, es decir, el hilo de UI estaba bloqueado. La duplicación se recreó, pero no llegó ningún frame.
  - Coincidió con un bloqueo general del equipo: el editor (VS Code) se cerró de golpe en esos minutos.
  - No se pudo atribuir a la captura, pero queda registrado. **El consumidor necesita un *watchdog*** que recree la captura si pasa demasiado tiempo sin frames, con el contenido cambiando.

### Otros hallazgos

1. **El monitor IDD aparece en el adaptador de la Iris Xe**, no en un adaptador propio del VDD. `DXGI_OUTPUT_DESC.DeviceName` coincide con el nombre GDI de `cs-display`. Por eso el dispositivo D3D11 de la captura es el de la Iris Xe, y el codificador QSV o MF podrá usar la misma textura sin copias entre adaptadores.
2. **`display show` provoca 1 o 2 `ACCESS_LOST` en los primeros 1–2 s.** Windows sigue asentando el modo y la topología después de que vuelve `SetDisplayConfig`/`ChangeDisplaySettingsEx`. El agente debe tolerarlo, y ya lo hace. Al medir, conviene descartar los primeros segundos.
3. **Un proceso solo puede duplicar una salida una vez.** Una segunda llamada a `DuplicateOutput1` sobre la misma salida devuelve `E_INVALIDARG`, que se traduce a `CaptureError::AlreadyCapturing`. Por eso las pruebas de integración se serializan. Implica que el agente debe tener **una única captura por monitor** y compartir sus frames.
4. **El formato fue siempre BGRA8.** No hay HDR en este equipo, así que la rama FP16 no se ha ejercitado.
5. **`DesktopImageInSystemMemory = false`:** la imagen está realmente en la GPU.
6. **El GDI name puede cambiar entre sesiones** (`DISPLAY12` antes y `DISPLAY13` después del bloqueo). Hay que resolverlo siempre con `cs-display` y no guardarlo.
7. **El cursor llega como forma `Color` de 32×32** sobre el monitor virtual y `Monochrome` sobre el panel. La posición solo se informa en la salida donde está el cursor (`visible = false` en las demás).

## Pruebas

- **17 pruebas unitarias** independientes del SO: rectángulos y fracción cambiada, nombres GDI, UTF-16, conversión QPC sin desbordamiento, percentiles, BMP y decodificación de cursores monocromo, color y color con máscara.
- **5 pruebas de integración `#[ignore]`** (`cargo test -p cs-capture -- --ignored`): enumeración, primer frame de la principal con contenido real, salida inexistente, reutilización del pool y segunda duplicación rechazada. Pasan con la sesión de escritorio.
- `cargo fmt` y `clippy -D warnings --all-targets` sin avisos.

## Pendiente

1. **Repetir las tandas con el equipo en reposo** (sin compilaciones paralelas) y con prioridad MMCSS en el hilo de captura. Medir la copia en GPU con `D3D11_QUERY_TIMESTAMP`.
2. **GPU dedicada y portátil híbrido (riesgo R8):**
   - comprobar en qué adaptador aparece la salida del IDD;
   - comprobar si `DuplicateOutput1` devuelve `DXGI_ERROR_UNSUPPORTED` desde el adaptador equivocado (ya se traduce a un error explícito).
3. **Windows Graphics Capture como *fallback*:**
   - cuando Desktop Duplication no esté disponible o falle de forma persistente;
   - comparar su latencia.
4. **Escritorio seguro (UAC, Ctrl+Alt+Supr, bloqueo):**
   - verificar que `E_ACCESSDENIED` se trata como transitorio y que la captura vuelve al salir;
   - un proceso de usuario no puede capturar el escritorio seguro: hará falta un mensaje en el receptor.
5. **HDR / FP16:** probar en un panel HDR y decidir la conversión a SDR antes del codificador.
6. ***Watchdog* de "sin frames con contenido cambiando"** (ver la tanda anómala) y suspensión/reanudación con captura activa.
7. **Compartir la textura con otro proceso** (`MISC_SHARED_NTHANDLE` + *keyed mutex*), si el codificador no vive en el mismo proceso. Hoy no se necesita: la captura y la codificación van juntas en el agente.
