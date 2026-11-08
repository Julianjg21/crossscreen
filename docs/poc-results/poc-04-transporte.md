# PoC-4: Transporte QUIC con datagramas y FEC (resultados parciales)

**Estado:** parcial. Solo se ha medido en **loopback**. Faltan mediciones entre dos equipos (Gigabit y Wi-Fi), con CPU, y la comparación de controladores de congestión.

**Herramienta:** `crossscreen bench-serve` / `crossscreen bench` (`apps/cli/src/bench.rs`).
- Envía frames **sintéticos** del tamaño que corresponde al bitrate y los fps pedidos, con un "IDR" 4 veces mayor cada 2 s.
- Los fragmenta con FEC Reed-Solomon y los envía por datagramas QUIC con TLS 1.3 y certificados fijados.
- No captura ni codifica pantalla.

**Métrica principal: *frame RTT*.** Es el tiempo desde el envío del primer datagrama de un frame hasta recibir su confirmación del receptor.
- Incluye la red (ida y vuelta), el reensamblado y el FEC.
- No requiere sincronizar relojes.

## Entorno
- Windows 11 Pro 10.0.26200, mismo equipo (loopback 127.0.0.1).
- Build release, Rust 1.99.1, quinn 0.11 (proveedor criptográfico ring), FEC 10 %.
- Fecha: 2026-11-08.

## Resultados (60 fps, 5 s)

| Bitrate pedido | Throughput real (con cabeceras) | Frames | Perdidos | Frame RTT p50 | p95 | p99 | máx |
|---|---|---|---|---|---|---|---|
| 20 Mbps | 23,4 Mbps | 300/300 | 0 | 0,95 ms | 1,44 ms | 2,30 ms | 3,94 ms |
| 80 Mbps | 93,0 Mbps | 300/300 | 0 | 2,57 ms | 3,52 ms | 5,67 ms | 9,20 ms |

El throughput supera al pedido por las cabeceras (24 B por datagrama), el 10 % de paridad FEC y los IDR 4 veces mayores.

## Hallazgo: buffers del socket UDP

La primera versión perdía **10 de 30 frames incluso en loopback**. Las estadísticas mostraban frames enteros perdidos sin ningún shard recuperado: ráfagas que superaban lo que cubre la paridad.

- **Causa:** el buffer de recepción UDP por defecto de Windows (64 KB) se desborda con la ráfaga de un solo frame de ~30 KB más los siguientes.
- **Corrección:** crear el socket con `socket2` y `SO_RCVBUF`/`SO_SNDBUF` de 8 MB (`crates/cs-transport/src/endpoint.rs`).
- **Resultado:** 0 pérdidas en 5 de 5 ejecuciones del test de integración.
- **En Linux:** el valor efectivo queda limitado por `net.core.rmem_max`. El código lo registra y la instalación deberá documentarlo o ajustarlo.

## *Pacing* dentro del frame (loopback)

**Implementación:** `crates/cs-transport/src/pacer.rs`, opción `--pacing <fracción|off>` (por defecto `0.6`) y `--pacing-wait` en `crossscreen bench`.
- **Planificador sans-IO:** *token bucket* en bytes con el tiempo inyectado. Para cada frame calcula el instante de envío de cada datagrama. Los primeros 4 datagramas salen al instante y el resto se reparte hasta el final de la ventana (60 % del intervalo = 10 ms a 60 fps). El *bucket* se conserva entre frames: si un IDR no ha terminado de salir, el frame siguiente va detrás y no se solapa. Tiene tests deterministas.
- **Ejecución en micro-ráfagas:** al despertar se envía todo lo que vence antes de `ahora + 250 µs` y se vuelve a esperar. El temporizador solo necesita precisión de ~250 µs, no la separación entre datagramas (~65 µs a 80 Mbps).
- **Espera elegida: temporizador *waitable* de alta resolución** (`CreateWaitableTimerExW` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) en un hilo de `spawn_blocking`. Ese hilo llama directamente a `send_datagram`, que en quinn es síncrono.

### Precisión y CPU de cada forma de esperar

Ejemplo `cargo run --release -p cs-transport --example pacer_timing -- <Mbps> 60 0.6 5`. Sin red: el "envío" solo anota la hora. El retraso es el instante real menos el planificado; es negativo cuando el envío se adelanta dentro de la micro-ráfaga. La CPU es la del proceso entero respecto al tiempo real.

| Espera | Mbps | Retraso p50 | p99 | máx | Fin real del frame (objetivo 10 ms) | Esperas/frame | CPU |
|---|---|---|---|---|---|---|---|
| Temporizador de alta resolución | 80 | 51 µs | 622 µs | 7,2 ms | 10,10 ms | 16,4 | 3,1 % |
| `std::thread::sleep` | 80 | 48 µs | 531 µs | 1,1 ms | 10,11 ms | 16,6 | 5,3 % |
| `yield_now` en bucle | 80 | −65 µs | 19 µs | 4,6 ms | 9,97 ms | 37,6 | 64,4 % |
| `tokio::time::sleep` | 80 | 10,2 ms | 16,5 ms | 24,1 ms | 15,69 ms | 1,0 | ~0 % |
| Temporizador de alta resolución | 20 | 93 µs | 675 µs | 1,4 ms | 10,41 ms | 15,6 | 4,1 % |
| `std::thread::sleep` | 20 | 61 µs | 653 µs | 5,0 ms | 10,42 ms | 16,4 | 3,1 % |
| `yield_now` en bucle | 20 | 0 µs | 24 µs | 1,5 ms | 10,37 ms | 35,9 | 65,8 % |
| `tokio::time::sleep` | 20 | 9,8 ms | 15,6 ms | 22,0 ms | 15,56 ms | 1,0 | ~0 % |

**Conclusiones:**
- **`tokio::time::sleep` no sirve en Windows.** Despierta ~10–16 ms tarde (la resolución del temporizador del sistema es de 15,6 ms), así que el frame sale en 1–2 ráfagas y termina tarde.
- **La espera activa con `yield_now` es la más precisa, pero cuesta ~65 % de un núcleo.** Se descarta.
- **El temporizador de alta resolución y `std::thread::sleep` empatan** (Rust ≥ 1.75 ya usa el mismo temporizador de alta resolución en Windows). Se deja el temporizador explícito para no depender de un detalle interno de `std`; en Linux se usa `std::thread::sleep` (nanosleep).
- Los máximos de varios ms vienen de que el planificador del SO desaloja el hilo. Con la máquina cargada (otros agentes compilando) una primera ejecución dio p99 de 7–40 ms en todas las estrategias. Estas cifras son de una segunda ejecución, ya sin compilaciones en marcha.

### Banco extremo a extremo con y sin *pacing*

`crossscreen bench` en loopback, 60 fps, 10 s (600 frames), FEC 10 %, build release, mismo equipo y fecha que arriba. El receptor (`bench-serve`) corre en otro proceso. Los rangos son el mínimo y el máximo de dos ejecuciones. En la primera de las filas de 80 Mbps aún había compilaciones de otros procesos en marcha; las demás se hicieron con la máquina en reposo. Las filas con una sola cifra son de una única ejecución.

| Bitrate | *Pacing* | Perdidos | Frame RTT p50 | p95 | p99 | Duración del envío del frame p50 | CPU del emisor |
|---|---|---|---|---|---|---|---|
| 20 Mbps | off | 0/600 | 0,61–1,02 ms | 0,94–1,57 ms | 1,38–2,18 ms | 0,02 ms | 4,2–5,5 % |
| 20 Mbps | 0,6 (alta res.) | 0/600 | 9,47–9,51 ms | 10,54–10,58 ms | 10,78–10,97 ms | 10,34 ms | 19,7–21,1 % |
| 80 Mbps | off | 0/600 | 1,55–2,69 ms | 3,48–3,73 ms | 5,01–6,36 ms | 0,01–0,02 ms | 18,5–26,0 % |
| 80 Mbps | 0,6 (alta res.) | 0/600 | 10,15–10,17 ms | 10,60–10,70 ms | 11,07–11,72 ms | 10,11–10,15 ms | 28,3–31,4 % |
| 80 Mbps | 0,6 (`std::thread::sleep`) | 0/600 | 10,15 ms | 10,53 ms | 10,72 ms | — | 23,1 % |
| 80 Mbps | 0,6 (`yield_now`) | 0/600 | 10,07 ms | 10,39 ms | 10,66 ms | — | 87,9 % |

La CPU es tiempo de usuario + núcleo del proceso emisor completo (`GetProcessTimes`) dividido por la duración del envío, en % de un núcleo. Incluye QUIC/TLS, FEC y el hilo de *pacing*.

**Lectura:**
- **El *frame RTT* con *pacing* incluye la ventana de reparto.** Se mide desde el primer datagrama, así que sube a ~10 ms por diseño.
- **El tiempo desde el último datagrama hasta el ACK baja a 0,15 / 0,69 / 1,33 ms (p50/p95/p99) a 80 Mbps**, frente a 1,51 / 3,35 / 4,92 ms en ráfaga: las colas ya no se llenan.
  - A 20 Mbps esta métrica sale ≈ 0: el receptor completa el frame con los shards de datos antes de que salgan los últimos shards de paridad, que van al final.
- **Coste en CPU:** el *pacing* añade ~5–15 puntos de un núcleo. Solo ~3–4 puntos son del temporizador (tabla anterior). El resto se atribuye, sin haberlo perfilado, a que quinn despierta y emite por cada micro-ráfaga en vez de una vez por frame, y agrupa peor los paquetes por llamada al sistema.
- **En loopback no hay pérdidas ni con ráfagas**, gracias a los buffers de 8 MB. Por eso **loopback no puede mostrar el beneficio real del *pacing*.** Ese beneficio está en no desbordar los buffers del AP Wi-Fi o de un conmutador, y hay que medirlo entre dos equipos por Wi-Fi.
- Con *pacing*, la `cwnd` de quinn se queda en 20 KB–1,4 MB en lugar de 15–100 MB: el emisor está limitado por la aplicación y la ventana no crece. No afectó al envío en loopback; hay que vigilarlo en Wi-Fi.

## Pendiente para cerrar la PoC-4
1. Repetir entre dos equipos por Ethernet Gigabit y por Wi-Fi 5/6 (`bench-serve` en uno, `bench` en otro).
2. Medir la CPU por proceso a 20, 50 y 80 Mbps.
3. ***Pacing* dentro del frame.** *(Implementado y medido en loopback: ver la sección anterior. Falta medir el beneficio por Wi-Fi.)* Antes cada frame se enviaba en ráfaga. En Windows, `tokio::time::sleep` tiene una granularidad de ~1–15 ms, así que hará falta un *pacer* por bytes (presupuesto de tokens) o temporizadores de alta resolución.
4. Comparar los controladores de congestión de quinn (NewReno, Cubic, BBR) bajo pérdida simulada (`clumsy` en Windows, `tc netem` en Linux).
5. Probar la tolerancia a pérdidas con FEC al 10, 20 y 30 % y pérdida inducida del 1–5 %.
