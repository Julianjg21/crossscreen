# MVP F2: receptor (`crossscreen-viewer --listen`)

**Estado:** funciona de extremo a extremo en Windows, tanto con un emisor de prueba como con el emisor real (`crossscreen stream`, modo *extend*). El receptor cumple el contrato `docs/09-contrato-sesion-mvp.md`: QUIC con huellas fijadas → canal de control → reensamblado → decodificación H.264 por hardware → presentación D3D11 sin copias, con el cursor fuera de banda. Se recupera de pérdidas con `RequestKeyframe`.
**Fecha:** 2026-11-08.
**Entorno:** el mismo de la PoC-5 (portátil con Intel Iris Xe como única GPU, panel 1920×1080 a 60 Hz con escala 125 %). Binarios `--release` y fuente y receptor en el mismo equipo, por `127.0.0.1`.

## Uso

```text
crossscreen-viewer --listen 0.0.0.0:47810 [--data-dir D] [--fullscreen] [--monitor N]
                   [--size 1920x1080] [--once] [--seconds S] [--same-host-latency]
                   [--screenshot p.png --screenshot-frame N]
```

- Solo acepta pares del `TrustStore` (`PeerPolicy::pinned`). Si no hay ninguno, sale con un mensaje que explica cómo emparejar (`crossscreen pair-listen` / `pair`).
- `--file x.h264` (el modo de la PoC-5) sigue funcionando.
- Sin `--once`, al terminar una sesión cierra la ventana y vuelve a escuchar (verificado con dos sesiones seguidas).

## Diseño

### Tres hilos

| Hilo | Qué hace | Nunca espera a… |
|---|---|---|
| **Red** (`net.rs`, tokio con 2 *workers*) | Endpoint QUIC, canal de control, `Reassembler`, coalescencia de `RequestKeyframe`, `ReceiverStats` cada 1 s | La presentación. Pasa el vídeo con `try_send` a una cola acotada de 16 AU; si se llena, cuenta pérdida y pide IDR. Los avisos van a la ventana con `EventLoopProxy::send_event`, que no bloquea |
| **Decodificación** (`decode_worker.rs`) | Decodifica **todas** las AU en orden con el MFT, sobre el **mismo** `ID3D11Device`, y deja cada frame en el buzón | La ventana |
| **Ventana** (`listen.rs`, winit) | Espera al objeto *waitable* de la cadena, toma el frame más reciente del buzón y dibuja vídeo + cursor | — |

**Por qué un hilo de decodificación propio:** el buzón de una plaza («el último gana») solo es correcto con frames **ya decodificados**. Un P-frame comprimido no se puede descartar sin romper las referencias, así que todo se decodifica en orden y solo se descartan frames decodificados que nadie llegó a presentar.

Si se decodificara en el hilo de la ventana, la decodificación quedaría atada al ritmo del *vblank*. Tras un hipo de red, la ráfaga de AU retrasaría la presentación varios refrescos.

El dispositivo D3D11 tiene la protección multihilo activa, y la textura NV12 del MFT se muestrea desde el otro hilo sin copias (zero-copy: sí, en todas las ejecuciones). Para pasar el `DecodedFrame` entre hilos se añadió `unsafe impl Send` en `cs-decode`, con la justificación en un comentario `SAFETY`: el `IMFSample` de MF es *free-threaded*.

### Buzón de una plaza

`session::Mailbox` es un `Mutex<Option<MailFrame>>`. `put` sustituye el frame anterior, que devuelve su superficie al MFT al soltarse, y `take` lo vacía. La ventana conserva el último frame presentado (`current`) para:
- repintar el cursor sin vídeo nuevo;
- mantener en pantalla el último frame bueno durante una pérdida.

Así, como mucho, se retienen 2 superficies del *pool* del MFT, que tiene 6 (1920×1088×6). No ha dado problemas.

### Control (contrato, pasos 1–6)

- `Hello` de la fuente → comprobación de versión mayor (si no coincide: `Bye { IncompatibleVersion }`).
- `Hello` propio con `roles = sink`.
- `SinkCapabilities` con H.264 por hardware (perfil High, `max_pixel_rate` de 2160p60, verificado en la PoC-5) y `native_mode` real:
  - a pantalla completa, el tamaño y el refresco del monitor elegido;
  - en ventana, `--size` (1920×1080 por defecto) con el refresco del monitor.
  - Los `dpi` salen de la escala del monitor: 96 × 1,25 = 120.
- `StartStream`: se valida `stream_id = 1`, el códec H.264 y el modo; si algo falla, `StreamFailed`.
- Se piden a la ventana la ventana, el renderer y el decodificador. Cuando están listos se envía `StreamStarted`.
- Si llegan mensajes de cursor antes de `StartStream`, se guardan y se aplican al empezar.

### Recuperación

En el hilo de decodificación hay un estado `awaiting_idr`, que empieza a `true`. Pasa a `true` con:
- `Lost` del reensamblador;
- un **hueco en `frame_seq`** (ver «Problemas», 1);
- un error del decodificador (además, `flush`);
- un frame no IDR al principio.

Mientras dura, los P-frames se descartan **sin entregarlos al MFT**, se pide `RequestKeyframe` y la ventana sigue mostrando el último frame bueno.

La red coalesce las peticiones (`KeyframeLimiter`): como máximo una cada 100 ms, y las intermedias no se pierden, se envían al cumplirse el intervalo. Cuando se decodifica el IDR, la petición pendiente se cancela. Sin esa cancelación, cada pérdida provocaba 2 IDR (14 peticiones para 7 pérdidas en la primera prueba).

### Cursor

- `cs-render` tiene ahora `Renderer::set_cursor_shape(w, h, hotspot, rgba)`. Sube el RGBA, **premultiplicado** en CPU, a una textura inmutable `R8G8B8A8` y lo dibuja con el *pixel shader* `ps_cursor_tex` y mezcla `ONE / INV_SRC_ALPHA`. Así se filtra sin halos al escalar.
- La posición, en píxeles del monitor transmitido, se escala igual que el vídeo (`cs_render::cursor_rect`, con test) y se le resta el punto caliente.
- La flecha procedimental queda solo como respaldo hasta que llega la primera forma. El cursor está oculto hasta el primer `CursorPosition`.
- Los mensajes de cursor se aplican al instante, sin esperar a un frame de vídeo.

**Hallazgo: los repintados de cursor robaban el hueco de la cadena.** Con `SetMaximumFrameLatency(1)`, cada `Present` ocupa el hueco hasta el siguiente *vblank*.

El emisor manda `CursorPosition` justo antes de cada frame. La ventana repintaba el cursor nada más llegar el mensaje, y el frame de vídeo, unos ms después, tenía que esperar al siguiente *vblank*. Se medía:
- 475 de 600 frames presentados (47 fps);
- 125 frames descartados en el buzón;
- `Present`→*vblank* de 33 ms.

**Solución (*late latching*):** un repintado que solo mueve el cursor se programa para 4 ms antes del siguiente *vblank*. El *vblank* se extrapola del `SyncQPCTime` de `GetFrameStatistics` con `Renderer::time_to_next_vblank`. Si entretanto llega un frame de vídeo, ambos salen en el mismo `Present`.

Resultado: 592 de 600 frames presentados (59,1 fps), 7 repintados solo de cursor y `Present`→*vblank* de 16,4 ms. winit 0.30 espera `WaitUntil` con un temporizador de alta resolución, así que la planificación es precisa.

### Latencia en el mismo equipo

Con `--same-host-latency`, por cada frame presentado se calcula `QPC_µs(tras Present) mod 2³² − pts_us` con aritmética *wrapping*. Si la diferencia es ≥ 2³¹ µs se descarta como «no comparable» y se cuenta aparte (`clock::latency_since`, con test de la vuelta de 2³²). Sin el parámetro no se calcula ni se muestra.

**Qué es «presentación» aquí:** el instante en que `Present()` devuelve el control, como define el contrato. El fotón llega en el siguiente *vblank* que compone DWM. Esa parte se informa por separado como «`Present()`→*vblank*» y no se suma.

### Métricas

**Informe al final de cada sesión:**
- fps reensamblados y presentados;
- frames perdidos, y de ellos cuántos sin ningún datagrama;
- shards recuperados por FEC;
- desbordes de la cola;
- errores de decodificación, frames descartados esperando IDR, `RequestKeyframe` enviados;
- latencias p50/p95/p99 de:
  - decodificación;
  - decodificado→`Present`;
  - reensamblado→`Present`;
  - `Present` en CPU;
  - `Present`→*vblank*;
  - recuperación (detección de la pérdida → IDR decodificado);
  - y, solo con `--same-host-latency`, captura→`Present`.

**`ReceiverStats` cada 1 s:**
- `frames_lost` incluye los huecos de `frame_seq`.
- `jitter_us` es el estimador de RFC 3550 sobre llegada frente a `pts`. Vale entre equipos porque el desfase de relojes se cancela.
- `decode_time_us` es el del último frame.
- `pre_fec_loss_bp` es una **estimación**: shards reconstruidos / datagramas recibidos. `ReassemblyStats` no cuenta la paridad perdida.

## Cómo se verificó

### Con el emisor de prueba (`apps/viewer/examples/file_source.rs`, desechable)

El emisor de prueba:
- habla el contrato completo;
- transmite el fichero de Quick Sync `bench_1080p.h264` (600 AU, 20 Mb/s, IDR cada 120) a 60 fps con `pts_us` = QPC en el momento de enviar;
- usa `Packetizer`, `Pacer` *spread* 0,6 y FEC 0,1;
- manda una flecha azul de 32×32 y, a los 5 s, una mira verde de 49×49 con el punto caliente en el centro, más la posición en círculo a 60 Hz;
- ante `RequestKeyframe` salta al IDR más reciente del fichero;
- puede descartar datagramas a propósito: `--loss P` o `--drop-every N`.

Es un banco de transporte y presentación: su `pts` **no incluye captura ni codificación**.

Emparejamiento: dos `--data-dir` en el scratchpad con `crossscreen pair-listen --bind 127.0.0.1:47901 --no-mdns` / `pair`. Todas las ejecuciones fueron en ventana de 1280×720 y duraron ≤ 20 s. Las capturas (`--screenshot`) se revisaron a mano: frame real, flecha azul con borde y sombra, cambio a la mira verde, y ningún artefacto tras las recuperaciones.

| Ejecución | Frames recibidos → presentados | Decode p50/p95 | Decodificado→Present p50/p95 | `Present`→vblank p50 | Captura→Present p50/p95/p99 (mismo equipo) |
|---|---|---|---|---|---|
| Sin pérdidas, antes del *late latching* | 600 → 475 (47,0 fps) | 0,79 / 1,80 ms | 16,3 / 18,0 ms | 32,7 ms | 27,6 / 28,5 / 29,1 ms |
| Sin pérdidas, con *late latching* | 600 → 592 (59,1 fps) | 0,85 / 1,24 ms | 15,4 / 16,3 ms | 16,4 ms | 25,9 / 26,2 / 26,4 ms |
| `--drop-every 90` (frames enteros) | 705 decodificados → 699 (58,2 fps) | 0,39 / 0,54 ms | 9,4 / 10,0 ms | 16,5 ms | 19,2 / 19,6 / 19,8 ms |
| `--drop-every 90 --loss 0.03`, FEC 0,1 | 698 decodificados → 676 (55,8 fps) | 1,06 / 2,15 ms | 14,7 / 16,1 ms | 32,7 ms | 26,9 / 28,0 / 29,2 ms |

**Recuperación tras pérdida inducida:**

| Prueba | Pérdidas detectadas | `RequestKeyframe` → IDR forzados | Recuperación p50 / p95 / máx. |
|---|---|---|---|
| `--drop-every 90` | 7 (7 sin ningún datagrama) | 14 → 14 (antes de cancelar la petición pendiente) | 17,6 / 17,8 / 17,8 ms |
| `--drop-every 90 --loss 0.03` | 10 (7 sin datagramas + 3 que el FEC no pudo reconstruir; otros 596 shards sí se reconstruyeron) | 10 → 10 | 18,2 / 26,4 / 26,4 ms |

Cómo leer estas cifras:
- La recuperación se mide desde que se **detecta** la pérdida hasta que el IDR está decodificado. Incluye la ida de la petición, el siguiente *tick* del emisor, el envío del IDR repartido en el 60 % del intervalo y la decodificación.
- La pérdida de un frame entero solo se detecta cuando llega el siguiente (~16,7 ms después). Por eso la congelación visible es de unos 2–3 frames (≈35–50 ms).
- «Decodificado→Present» incluye esperar el hueco de la cadena (hasta un *vblank*). Depende de la fase entre el reloj del emisor y el panel, que en 10 s apenas deriva. Por eso varía entre ejecuciones (9–15 ms de p50) aunque el trabajo real sea pequeño: `Present()` en CPU tiene un p50 de 0,2–0,6 ms y decodificar, < 1,1 ms.
- `Present`→*vblank* osciló entre 16,4 y 32,7 ms de una ejecución a otra con el mismo binario. En las de 32,7 ms podía estar activo el monitor virtual del otro agente (`\\.\DISPLAY13`, visible en la enumeración). No está confirmado que esa sea la causa.

### Con el emisor real (`crossscreen stream`, del otro agente)

Visor en ventana de 1280×720 en el panel principal; emisor `crossscreen --data-dir B stream 127.0.0.1:47810 --peer … --width 1920 --height 1080 --hz 60 --seconds 12` (modo *extend*: crea `\\.\DISPLAY13` y lo retira al terminar).

- **Primer intento:** la interoperabilidad funcionó entera (Hello, `SinkCapabilities`, `StartStream` *Extend*, `StreamStarted`, IDR, cursor, `ReceiverStats` y `Bye`). Pero la captura del **emisor** dio `ACCESS_LOST` y no se recuperó en 5 s. El emisor terminó con `Bye { Error }` y el visor cerró la sesión limpiamente. El fallo es del lado emisor.
- **Segundo intento (12 s):**
  - 132 frames recibidos, decodificados y presentados (11 fps: el escritorio virtual está casi estático y el emisor solo captura cuando hay cambios), 0 perdidos.
  - La captura de pantalla muestra el fondo de escritorio del monitor virtual.

| Métrica (receptor, mismo equipo) | p50 | p95 | p99 |
|---|---|---|---|
| Decode (AU→salida del MFT) | 0,95 ms | 2,14 ms | 2,61 ms |
| Decodificado→`Present` devuelto | 4,6 ms | 10,1 ms | 17,2 ms |
| Reensamblado→`Present` devuelto | 5,9 ms | 12,5 ms | 32,2 ms |
| `Present()`→*vblank* | 32,9 ms | 33,0 ms | 35,6 ms |
| **Captura→`Present` devuelto (n = 128)** | **27,8 ms** | **38,5 ms** | **52,1 ms** |

El emisor informó de una latencia captura→enviado de 21,6 / 33,7 ms (p50 / p95), con una codificación de ~10 ms de p50. En los 27,8 ms de captura→presentación, unos 22 ms son del emisor y ~6 ms del receptor. El fotón llega un `Present`→*vblank* después, ~33 ms en esta ejecución en ventana.

## Problemas encontrados

1. **El `Reassembler` no detecta frames perdidos por completo.** Solo declara `Lost` para frames de los que recibió algún fragmento. Si se pierden **todos** los datagramas de un frame, no hay evento. Es lo habitual con P-frames pequeños (1–2 datagramas).
   - En la primera prueba con `--drop-every 90` se descartaron 8 frames, se informó de 0 perdidos y los P-frames huérfanos se decodificaron sin error: el MFT oculta el fallo, con corrupción visual.
   - **Solución en el receptor:** `net::SeqTracker` detecta huecos en `frame_seq`, que el contrato hace consecutivo, y los trata como `Lost`.
   - **Propuesta:** moverlo a `cs-transport` (no es de este agente).
2. **Los repintados de cursor robaban el hueco de la cadena:** ver «Cursor». Se resolvió con *late latching*.
3. **Peticiones de IDR redundantes:** la petición coalescida quedaba pendiente y salía 100 ms después aunque el IDR ya hubiera llegado. Se cancela al decodificar el IDR.
4. **El emisor de prueba solo llegaba a 31 fps** con `tokio::time::interval`: en Windows tiene una granularidad de ~15,6 ms. Se cambió a una espera de alta resolución (`cs_transport::os::high_res_sleep`). Solo afectaba al ejemplo.
5. **`ACCESS_LOST` sin recuperar en el emisor real** (primer intento extremo a extremo). Es del lado emisor y se lo comunico al otro agente.

## Cuestiones del contrato (sin cambiar `cs-protocol`)

- **Frames perdidos por completo:** convendría que el contrato o `cs-transport` dijeran explícitamente que un hueco en `frame_seq` cuenta como `Lost` (ver «Problemas», 1).
- **Intervalo de `ReceiverStats`:** el comentario de `ControlMessage::ReceiverStats` en `cs-protocol` dice «cada ~100 ms» y el contrato, «cada ~1 s». Se implementó 1 s, como dice el contrato.
- **`pre_fec_loss_bp`** no se puede calcular con exactitud con `ReassemblyStats`, que no cuenta los shards de paridad perdidos. Hoy es una estimación.
- **Definición de «presentación»:** en ventana con DWM, el fotón llega 1–2 refrescos después de `Present()`. El contrato podría fijar si la latencia captura→presentación se mide en `Present()` o en el *vblank* de `GetFrameStatistics`. Aquí se usa `Present()` y el *vblank* se informa aparte.
- **IDR redundantes:** el emisor podría ignorar los `RequestKeyframe` que lleguen menos de ~1 RTT después de un IDR forzado. El receptor ya evita la mayoría.

## Iteración de resiliencia en Wi-Fi / pérdidas en ráfaga

### Diagnóstico de la primera prueba real entre 2 equipos por Wi-Fi (`v0.0.1-alpha.1`)

En la primera sesión de 60 s entre el portátil de desarrollo y un segundo PC por Wi-Fi (`1366×768@60`, `Duplicate`), el registro del emisor mostró:
- 996 frames enviados (~16,6 fps), 899 recibidos, **31 frames perdidos post-FEC**, 711 shards recuperados por FEC (`pre_fec_loss_bp` hasta 190, ~1,9 %);
- RTT inicial de **943,3 ms** y *jitter* de **38,4 ms**;
- **104 `RequestKeyframe` recibidos** y **90 IDR forzados** (~1,5 por segundo, el **9 % de todos los frames**), con un tamaño medio de **79,5 KiB por IDR** (~68 datagramas) frente a **36,7 KiB por P-frame** (~32 datagramas).

Se produjo un bucle de realimentación positiva:
1. En cuanto faltaba un solo P-frame, el receptor descartaba todos los P-frames siguientes hasta recibir un IDR (**congelación deliberada**).
2. `KeyframeLimiter` reenviaba `RequestKeyframe` cada **100 ms** fijos mientras seguía esperando el IDR, un plazo menor que el RTT real en Wi-Fi con cola, disparando varias peticiones por cada pérdida.
3. Además, `SeqTracker` no avanzaba `last_seq` cuando `Reassembler` emitía `ReassemblyEvent::Lost { frame_seq }`, por lo que al llegar el siguiente frame completo se volvía a contar un hueco falso y se duplicaba el aviso de pérdida.
4. Cada IDR forzado duplicaba los datagramas en vuelo y tenía más del doble de probabilidad de sufrir una ráfaga Wi-Fi, prolongando la congelación.

### Cambios en el receptor (`apps/viewer`)

1. **Decodificar a través de la pérdida por defecto (`LossPolicy::DecodeThrough`):**
   - Tras el primer IDR de la sesión, si se pierde un P-frame se avisa a la red para pedir un IDR de saneamiento, pero **se siguen entregando los P-frames siguientes al MFT de Intel**.
   - El decodificador hardware aplica ocultación de errores (*error concealment*) sobre los macrobloques cuya referencia faltó mientras el resto de la pantalla y el cursor siguen moviéndose a ~60 fps.
   - Si el MFT devolviese un error en un frame concreto, solo entonces se hace `flush()` y se espera al IDR.
   - El comportamiento anterior se conserva detrás de `--freeze-on-loss` para comparar en pruebas.
2. **Política calmada de `RequestKeyframe` (`KeyframePolicy::Calm`, `KeyframeRequester`):**
   - Envía **una sola petición** al abrirse un episodio de pérdida y suprime todos los avisos adicionales de pérdidas consecutivas mientras el episodio sigue abierto.
   - No reintenta hasta que vence `max(3 · RTT, 250 ms) · 2^(intentos − 1)` (con tope en 2 s), o hasta que se decodifica un IDR con `frame_seq > seq_en_la_petición` (que cierra el episodio).
   - `--keyframe-policy legacy` mantiene el limitador fijo de 100 ms para comparar.
3. **Corrección de `SeqTracker` (`net::SeqTracker::note_lost`):**
   - Cuando `Reassembler` emite `Lost(seq)`, `SeqTracker` avanza `last_seq = seq` para no volver a contar ese mismo frame como hueco al llegar `seq + 1`.
4. **Métricas de fluidez y episodios:**
   - Se miden las **congelaciones visibles** (intervalos **> 200 ms** entre frames presentados consecutivos: cuenta, suma y máximo), los frames decodificados/presentados con referencias rotas (`concealed`), los avisos de pérdida absorbidos sin enviar petición nueva y las estadísticas por episodio de pérdida (peticiones y frames decodificados mientras llega el IDR).
5. **Simulador de pérdidas en ráfaga Gilbert-Elliott en `file_source`:**
   - Añadidos `--ge-p`, `--ge-r`, `--ge-loss` y `--seed`, además de `--keyframe-fec`, `--interleave`, `--adaptive-fec`, `--coalesce-ms` y `--sim-rtt-ms`.

### Comparativa medida bajo pérdidas en ráfaga Gilbert-Elliott

Condiciones comunes: flujo `bench_1080p.h264` (1920×1080, 600 AUs codificadas a 20 Mbps con el MFT de Intel, IDR cada 120 frames de ~75 KiB y P-frames de ~33 KiB) transmitido a **60 fps durante 15 s (900 frames)** en *loopback* con el mismo proceso de pérdidas Gilbert-Elliott (`p = 0,015`, `r = 0,25`, `h = 0,8`, `seed = 24301`: **~4,7 % de datagramas perdidos en ~470–580 ráfagas** de media 4 datagramas).

| Configuración | Frames reensamblados | Frames perdidos | Shards recup. FEC | Frames decodif. (`concealed`) | Frames descartados esperando IDR | Frames presentados (fps) | Congelaciones > 200 ms (total / máx.) | `RequestKeyframe` enviados (suprimidos) | Peticiones / episodio (media / máx.) |
|---|---:|---:|---:|---:|---:|---:|---|---:|---|
| **1. Receptor antiguo** (`--freeze-on-loss --keyframe-policy legacy`, emisor FEC 0,1 fijo) | 767 (51,1 fps) | 133 | 460 | 550 (0) | **217** | **545 (36,3 fps)** | **3 (867 ms / 367 ms)** | **94** (0) | 1,24 / 4 |
| **2. Receptor nuevo** (`DecodeThrough + Calm`, mismo emisor FEC 0,1 fijo) | 766 (51,0 fps) | 134 | 424 | **766** (396) | **0** | **659 (43,9 fps)** | **0 (0 ms / 0 ms)** | **72** (81) | 1,37 / 4 |
| **3. Emisor nuevo + Receptor nuevo** (FEC P=0,15 adaptativo, IDR=0,5, entrelazado + `DecodeThrough + Calm`) | **886 (59,0 fps)** | **14** | **1082** | **886** (12) | **0** | **853 (56,8 fps)** | **0 (0 ms / 0 ms)** | **12** (2) | **1,00 / 1** |

Conclusiones de la medición:
- **Solo con el cambio en el receptor** (filas 1 → 2, manteniendo el emisor antiguo sin entrelazar):
  - Desaparecen por completo los **217 frames descartados esperando IDR** y las **3 congelaciones > 200 ms (867 ms acumulados)**.
  - Todos los 766 frames reensamblados pasan por el MFT con **0 errores y 0 atascos** (decodificación p50 = 0,74 ms, p95 = 1,53 ms); la tasa de presentación sube de **36,3 a 43,9 fps** (+106 frames sustituidos en el buzón por llegar antes del siguiente *vblank*).
  - Las peticiones `RequestKeyframe` bajan un **23,4 %** (de 94 a 72), absorbiendo **81 avisos de pérdida** sin disparar peticiones duplicadas.
  - Se verificó visualmente con `--screenshot-concealed` que el MFT presenta el frame con ocultación localizada sin detener la animación ni el cursor.
- **Combinado con las mejoras del emisor** (fila 3):
  - Los frames perdidos caen un **89,5 %** (de 133 a **14 en 15 s**), las peticiones de IDR bajan un **87,2 %** (de 94 a **12**, exactamente **1,00 por episodio**, recuperadas en **18,1 ms p50 / 22,1 ms máx.**) y la presentación sube a **56,8 fps con 0 congelaciones** y solo 12 frames con ocultación menor en toda la sesión.

## Pendiente / no verificado

- `--listen --fullscreen` no se probó (todas las pruebas fueron en ventana para no ocupar la pantalla del usuario). Tampoco `--no-vsync` (tearing) en modo `--listen`: es la vía para bajar la latencia de presentación en ventana.
- La latencia captura→presentación con el emisor real se midió con poco movimiento: 128 muestras a 11 fps. Falta una medición con contenido a 60 fps (vídeo o arrastre de ventanas en el monitor virtual).
- Entre equipos distintos la latencia no se mide (por diseño). Para eso haría falta sincronizar relojes (`Ping`/`Pong`).
- Faltan los cambios de resolución en mitad de la sesión (el MFT los soporta con `STREAM_CHANGE`, pero no se ha probado por red) y la pérdida del dispositivo D3D11 (`DXGI_ERROR_DEVICE_REMOVED`): hoy cierra la sesión con error.
- Linux (VAAPI + DMA-BUF) sigue pendiente, como en la PoC-5.
