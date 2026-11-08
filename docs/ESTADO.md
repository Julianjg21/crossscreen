# Estado del proyecto (memoria compartida entre agentes)

> **Cómo usar este archivo:**
> - Léelo al empezar.
> - Al terminar tu sesión, actualiza "Trabajo en curso", "Próximos pasos" y añade una línea a la **Bitácora**: fecha, quién, qué se hizo y qué quedó a medias.
> - Las reglas permanentes están en [`AGENTS.md`](../AGENTS.md).

**Última actualización:** 2026-11-08, sesión principal (coordinador).

## Resumen en una frase
La cadena completa de Windows funciona de punta a punta (monitor virtual → captura → H.264 por hardware → QUIC cifrado → decodificación → presentación con cursor), **incluida una prueba real entre dos PCs por Wi-Fi**. El problema actual es la **resiliencia ante pérdidas en Wi-Fi**: hay congelaciones y saltos.

## Fases (ver `docs/07-roadmap.md`)
| Fase | Estado |
|---|---|
| F0 Fundamentos | ✅ En lo esencial (CI y release funcionando en GitHub) |
| F1 PoC (Windows) | ✅ PoC-1, 2, 3 y 5; PoC-4 en loopback + primera prueba real |
| F2 MVP Windows → Windows | 🟡 Funciona; falta resiliencia en Wi-Fi, prueba por cable, estabilidad de 2 h e integración en agente/panel |
| F3 Extender en Windows | 🟡 Adelantada (VDD + CCD funcionando) |
| F4 Linux | ⏳ No iniciada (no hay equipo Linux para probar) |

## Entorno de prueba real
- **PC-A (fuente, se extiende):** portátil del dueño `DESKTOP-6K2V76S`.
  - Intel Iris Xe, panel 1920×1080 al 125 %.
  - Wi-Fi `192.168.1.134`.
  - Driver VDD 25.7.23 **instalado**.
  - Huella `8075-2f40-0208-a313`.
- **PC-B (receptor, hace de pantalla):** `DESKTOP-93VFE2P`.
  - Panel 1366×768.
  - IP `192.168.1.127`.
  - Huella `ff81-e0ec-06c2-3137`.
  - Usa el `.zip` de la release, en `C:\Users\Julian\Downloads\crossscreen-0.0.1-windows-x64`.
- **A y B ya están emparejados**, cada uno en su carpeta de datos por defecto.
- **Cómo lanzar:**
  - En B: `.\crossscreen-viewer.exe --listen 0.0.0.0:47810 --fullscreen`
  - En A: `crossscreen stream 192.168.1.127:47810 --peer ff81 --seconds 60`
  - Guía completa: `docs/10-prueba-dos-equipos.md`.
- **Identidades de prueba para loopback en A:** `…/scratchpad/rx/A` (receptor, huella `8492…`) y `rx/B` (emisor, huella `772a…`), ya emparejadas entre sí. Viven en el scratchpad de la sesión de Claude Code y pueden desaparecer; si no existen, crea otras dos con `--data-dir` y empareja con `pair-listen` / `pair`.

## Resultados clave medidos
- **Mismo equipo (loopback):** captura → `Present()` p50 29 / p95 32 ms, 0 pérdidas.
- **Codificación MF en la Iris Xe:** 1080p60 p95 ≈ 11 ms (objetivo ≤ 6 ms: **no se cumple**); 1366×768 p50 6,7 / p95 9,4 ms.
- **Primera prueba real (Wi-Fi, 60 s, 1366×768):**
  - Funcionó; el dueño pudo manejar la segunda pantalla.
  - **Mucho lag, saltos bruscos y congelaciones.**
  - 31 frames perdidos (pérdidas en **ráfaga**: 0 datagramas tardíos, solo 18 shards recuperados con FEC 10 %).
  - 104 peticiones de IDR y 90 IDR forzados.
  - Recuperación de 0,4 a **6,4 s**; 213 frames descartados esperando IDR.
  - RTT inicial 943 ms y *jitter* 38 ms (probable ahorro de energía del Wi-Fi).
  - **Diagnóstico:** círculo vicioso. Una pérdida congela hasta el siguiente IDR; los IDR (~80 KB) también se pierden en ráfagas; las peticiones repetidas saturan el Wi-Fi.

## Trabajo en curso (2026-11-08)
Dos agentes en paralelo **sin hacer commit**: sus cambios quedan en el árbol de trabajo hasta que el coordinador los verifique y los suba.

1. **Receptor** (`apps/viewer`, `crates/cs-decode`, `crates/cs-render`):
   - decodificar a través de la pérdida en vez de congelar (`--freeze-on-loss` mantiene el modo antiguo);
   - política calmada de peticiones de IDR: una petición y esperar `max(3·RTT, 250 ms)` o hasta que llegue un IDR, con *backoff*;
   - quitar el doble aviso de `SeqTracker` (cs-transport ya emite `Lost` por huecos);
   - simulador de pérdidas en ráfaga (Gilbert-Elliott) en `apps/viewer/examples/file_source.rs`.

   Documenta en `docs/poc-results/mvp-f2-receptor.md`.
2. **Emisor y transporte** (`crates/cs-transport`, `crates/cs-pipeline`, `apps/cli`, `crates/cs-encode`):
   - FEC separado para IDR (~0,5);
   - intercalado de shards entre bloques FEC;
   - FEC adaptativo según `ReceiverStats`;
   - ignorar peticiones de IDR dentro de ~1 RTT del último;
   - probar el *intra refresh* del MFT de Intel y verificarlo en el stream;
   - opción de prueba `--sim-loss gilbert:…` en `crossscreen stream`.

   Documenta en `docs/poc-results/mvp-f2-emisor.md`.

**Si esta sesión se corta antes de integrarlo:**
1. Revisa `git status` / `git diff` para ver lo que dejaron.
2. Lee las secciones nuevas de esos dos documentos.
3. Pasa la verificación completa (ver `AGENTS.md`, punto 5).
4. Haz commit por tema y publica `v0.0.1-alpha.2`.

## Próximos pasos (en orden)
1. Integrar y verificar el trabajo en curso; publicar **`v0.0.1-alpha.2`**; repetir la prueba real A → B por Wi-Fi.
2. Probar **por cable** para separar los problemas del Wi-Fi de los del programa.
3. Pedir al dueño que desactive el ahorro de energía del Wi-Fi en los dos equipos y volver a medir.
4. Integrar el streaming en `crossscreen-agent` y en el panel Tauri (botón "Transmitir pantalla"; hoy está como "Próximamente").
5. **Latencia:**
   - visor a pantalla completa (en ventana, `Present()` → *vblank* ≈ 33 ms);
   - codificador más rápido (oneVPL/QSV directo; NVENC/AMF en otro hardware);
   - control de tasa (el MFT de Intel ignora el cambio de bitrate en caliente).
6. Prueba de estabilidad de 2 h; suspensión/reanudación con el monitor extendido.
7. **Linux (F4):** primero el receptor (VAAPI + wgpu), luego la fuente (Mutter/KWin/wlroots/EVDI + PipeWire). Requiere un equipo Linux real.

## Problemas conocidos y decisiones vigentes
- **VDD:** no usar su tubería. Modos y cantidad de monitores se cambian con el helper (XML + `pnputil /restart-device`, admin); mostrar y retirar con CCD (sin admin). Windows pone cada monitor nuevo en *duplicar*.
- **Codificador MF de Intel:**
  - necesita `timeBeginPeriod(1)` y excluir el proceso de la limitación de energía;
  - ignora el cambio de bitrate en caliente;
  - CBR rellena cada frame (~40 KB a 20 Mbps).
- **Visor:** D3D11 directo en Windows (no wgpu), sin copias.
- **Captura:** una sola duplicación por monitor y por proceso; el nombre GDI (`\\.\DISPLAYn`) cambia, así que hay que buscarlo siempre con `cs-display`.
- **El IPC de Linux y todo lo de Linux** compila (solo los crates portables), pero nunca se ha ejecutado en un Linux real.
- **Clave privada:** DPAPI en Windows; en Linux un archivo 0600 (Secret Service pendiente).

## Bitácora
| Fecha | Quién | Qué |
|---|---|---|
| 2026-11-08 | Coordinador | Análisis de las 5 referencias, arquitectura, ADR, roadmap y PoC (docs/). |
| 2026-11-08 | Coordinador | Núcleo: cs-core/protocol/security/transport/discovery/session + CLI; 55 tests. |
| 2026-11-08 | Coordinador | PoC-1: VDD instalado; tubería descartada (lo tumba); topología CCD; 100/100 ciclos. |
| 2026-11-08 | 5 agentes en paralelo | PoC-2 captura, PoC-3 codificación, PoC-5 visor, agente + IPC + panel Tauri, *pacing* + DPAPI. |
| 2026-11-08 | 2 agentes + coordinador | MVP F2: `crossscreen stream` + `crossscreen-viewer --listen`; arreglo de frames perdidos por completo en el reensamblador; mismo equipo p50 29 ms. |
| 2026-11-08 | Coordinador | Repo en GitHub, CI en verde, release `v0.0.1-alpha.1`; revocación de pares en caliente en el visor. |
| 2026-11-08 | Dueño + coordinador | **Primera prueba real A → B por Wi-Fi:** funciona, pero con lag y congelaciones por pérdidas en ráfaga. Lanzados 2 agentes de resiliencia (ver "Trabajo en curso"). |
