# CrossScreen

Plataforma open source para usar un segundo computador como **monitor adicional real** de otro a través de Ethernet o Wi-Fi, con soporte bidireccional Windows ⇄ Linux.

> Estado: **alfa.** Funcionan las tres direcciones entre equipos reales: **Windows → Windows**, **Windows → Linux** y **Linux → Windows** (extender o duplicar), con ratón y teclado remotos, portapapeles de texto opcional, reconexión automática y panel de escritorio en ambos sistemas.
>
> Guías: [Windows ↔ Windows](docs/10-prueba-dos-equipos.md) · [Linux (receptor y fuente)](docs/11-receptor-linux.md).

## Qué funciona hoy

| Componente | Estado | Verificación |
|---|---|---|
| `cs-core`: tipos de dominio, validación de modos y negociación de códecs | ✅ | Tests unitarios y `proptest` |
| `cs-protocol`: mensajes de control (postcard) y cabecera de vídeo de 24 B | ✅ | *Round-trip*, partición arbitraria del flujo y basura aleatoria sin pánico |
| `cs-security`: identidad Ed25519, emparejamiento SPAKE2, verificadores TLS con huella fijada, almacén de confianza, limitador de intentos | ✅ | Tests de PIN incorrecto y de MITM con PIN correcto |
| `cs-transport`: QUIC (quinn) con TLS 1.3 mutuo, *packetizer* y reensamblador con FEC Reed-Solomon, emparejamiento sobre QUIC | ✅ | Tests de integración en loopback real; rechazo de un par no fijado |
| `cs-discovery`: anuncio y exploración mDNS `_crossscreen._udp` | ✅ | Usado por el panel para encontrar equipos en la LAN |
| `cs-session`: máquina de estados con reconexión y gracia del monitor | ✅ | Tests de transiciones |
| CLI `crossscreen`: `id`, `pair-listen`, `pair`, `peers`, `forget`, `discover`, `bench-serve`, `bench` | ✅ | Probada de punta a punta en un equipo |
| Clave privada protegida por el SO | ✅ Windows (DPAPI, con migración) / ❌ Linux (archivo 0600; Secret Service pendiente) | Tests de ida y vuelta, migración y blob manipulado |
| *Pacing* de datagramas (*token bucket* con temporizador de alta resolución) | ✅ medido en loopback; ⚠️ falta Wi-Fi real | [PoC-4](docs/poc-results/poc-04-transporte.md) |
| `cs-capture` (Windows): DXGI Desktop Duplication del monitor virtual en GPU, cursor fuera de banda, recuperación ante cambio de modo | ✅ | [PoC-2](docs/poc-results/poc-02-captura.md): 0,64 ms p95 de CPU por frame |
| `cs-encode` (Windows): H.264 por hardware (Media Foundation, Intel QSV) con entrada D3D11 y BGRA→NV12 en GPU | ✅ funciona; ⚠️ **no cumple el objetivo de latencia en este portátil** (1080p60 p95 ≈ 11 ms frente a ≤ 6 ms) | [PoC-3](docs/poc-results/poc-03-codificacion.md) |
| `cs-decode` + `cs-render` + `crossscreen-viewer` (Windows): decodificación por hardware y presentación D3D11 sin copias | ✅ | [PoC-5](docs/poc-results/poc-05-decodificacion-render.md): decodificación + presentación ≈ 3,6 ms p95 en ventana |
| `cs-ipc` + `crossscreen-agent` + panel Tauri (`apps/desktop`) | ✅ Windows y Linux | Comunicación real agente ↔ panel; canal restringido al usuario actual |
| `crossscreen-helper` (Windows, admin bajo demanda): instala y desinstala Virtual-Display-Driver verificando el hash de cada archivo, y reinicia el dispositivo | ✅ | Instalado y recuperado un código 43 en este equipo ([PoC-1](docs/poc-results/poc-01-vdd.md)) |
| `cs-display` (Windows): mostrar y retirar el monitor virtual con CCD de rutas explícitas y fijar el modo, sin privilegios | ✅ | 100/100 ciclos sin fallos; `crossscreen display show\|hide\|list\|cycle` |
| Linux como receptor: `crossscreen-viewer --listen` con OpenH264 (binario de Cisco, por CPU) y wgpu (Vulkan/GL), X11 y Wayland | ✅ validado en Ubuntu real | Prueba Windows → Ubuntu completada; VAAPI (GPU) pendiente |
| Linux como fuente: **extender** (monitor virtual de GNOME, API de Mutter) y **duplicar** (XDG Desktop Portal + PipeWire), OpenH264 | ✅ validado en Ubuntu real | Duplicar y extender probados por el dueño; extender también contra Mutter 46.2 *headless* |
| Linux como fuente: **H.264 por GPU (VA-API)** con OpenH264 de respaldo, regulador de tasa por retardo de red y prioridad alta (`rtkit`) | ✅ medido; ⚠️ falta probarlo entre los dos equipos | Intel HD 520 a 1366×768: codificación p50 3,1 ms (OpenH264 9,8); con Wi-Fi malo simulado, retraso añadido p95 297 ms (antes 3 989); ADR-0005 |
| Cable directo sin router (IPv6 de enlace local, doble pila) | ✅ en loopback; ⚠️ falta probar con cable | En Windows, la red sin identificar es Pública: ver `docs/10-prueba-dos-equipos.md` |
| Extender en **Sway** (salida *headless* + `wlr-screencopy`) y en **KDE Plasma** (portal, fuente `VIRTUAL`) | ✅ Sway · ⚠️ KDE sin probar | Sway 1.9 *headless*: salida a la resolución del receptor, imagen exacta en Windows, retirada al terminar; ADR-0011 |
| Entrada remota hacia Linux (Mutter `RemoteDesktop` / portal) y portapapeles en Linux (X11/XWayland) | ✅ implementado; ⚠️ pendiente de probar en Ubuntu real | Inyección real contra Mutter 46.2; portapapeles entre procesos con Xvfb |
| **Audio del sistema** (opcional en emisor y receptor): WASAPI *loopback* / PipeWire, Opus en Rust puro, búfer adaptativo con compensación de deriva | ✅ | Linux → Windows de punta a punta: 795 tramas enviadas y reproducidas, 0 pérdidas; WASAPI real ida y vuelta; ADR-0008 |
| **Varios monitores por sesión** (opcional, solo extender): un monitor virtual, una conexión y un visor por pantalla, en puertos consecutivos | ✅ | Linux (GNOME *headless*) → 2 visores Windows y → agente Linux con 2 pantallas: imagen en ambos, 0 pérdidas; ADR-0009 |
| **Transferencia de archivos** (opcional en emisor y receptor): arrastrar al panel o al visor, streams QUIC, nombre saneado, sin sobrescribir | ✅ | Pruebas sobre QUIC real (ida y vuelta, nombres hostiles, cuerpos incompletos) y Linux → Windows de punta a punta; ADR-0010 |
| **HEVC y AV1** por hardware (opcional; por defecto H.264): negociados según lo que codifique el emisor y decodifique el receptor, con vuelta a H.264 | ✅ Windows ↔ Windows | HEVC 1080p ~65 fps sin pérdidas; AV1 recibido por hardware con un flujo real; codificar AV1 pendiente de una GPU que lo tenga; ADR-0012 |
| Reconexión automática del emisor (Windows y Linux) | ✅ | Corte de red de 12 s a mitad de transmisión: reconexión 1,9 s después de volver la red, sin pérdidas |
| Instalación en Linux: `.deb`, AppImage y `.tar.gz`; OpenH264 se instala desde el panel | ✅ | Descarga oficial de Cisco con SHA-256 verificado |
| Registros por ejecución (panel, agente, visor, CLI) con botón «Abrir carpeta de registros» | ✅ | `%LOCALAPPDATA%\CrossScreen\logs` · `~/.local/state/crossscreen/logs` |
| **Cadena completa (MVP F2, Windows):** `crossscreen stream` (emisor: monitor virtual → captura → codificación → QUIC) y `crossscreen-viewer --listen` (receptor: QUIC → decodificación → presentación con cursor real) | ✅ validado entre dos equipos por Wi-Fi | Captura → `Present()` en el mismo equipo: p50 29 ms / p95 32 ms; 0 frames perdidos ([emisor](docs/poc-results/mvp-f2-emisor.md), [receptor](docs/poc-results/mvp-f2-receptor.md)) |
| Integración del streaming en el agente y el panel | ✅ Windows y Linux | Validado por el dueño |

## Descargar y probar (alfa)

Desde **Releases**:
- **Windows x64:** `crossscreen-<versión>-windows-x64.zip` — guía [docs/10-prueba-dos-equipos.md](docs/10-prueba-dos-equipos.md). Sin firma de código todavía; no necesita el *Visual C++ Redistributable*.
- **Linux x86_64 (Ubuntu 22.04/24.04):** `CrossScreen_<versión>_amd64.deb` (panel instalable), `.AppImage` o `crossscreen-<versión>-linux-x64.tar.gz` — guía [docs/11-receptor-linux.md](docs/11-receptor-linux.md).

Para publicar una versión nueva: subir la versión en `Cargo.toml`, `apps/desktop/package.json` y `apps/desktop/src-tauri/tauri.conf.json` (el `.deb` solo se actualiza encima si la versión sube) y etiquetar `git tag v<versión>-alpha.N && git push origin v<versión>-alpha.N`. GitHub Actions compila en Windows y Ubuntu limpios y adjunta los paquetes a la release (`.github/workflows/release.yml`).

## Desarrollo

Requisitos: Rust estable. En Windows también hacen falta las MSVC Build Tools (carga de trabajo "Desarrollo de escritorio con C++").

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p crossscreen-cli -- id
```

Emparejar dos equipos y medir el transporte:

```sh
# Equipo B (muestra un PIN)
crossscreen pair-listen
# Equipo A
crossscreen pair <ip-de-B>:<puerto> <PIN>
# Banco de pruebas del transporte (PoC-4)
crossscreen bench-serve                         # en B
crossscreen bench <ip-de-B>:47800 --peer <huella-de-B> --mbps 20   # en A
```

> **¿Eres un agente o retomas el trabajo?** Empieza por [`docs/ESTADO.md`](docs/ESTADO.md) (en qué vamos) y [`AGENTS.md`](AGENTS.md) (reglas del proyecto).

## Terminología

| Término | Significado |
|---|---|
| **Fuente** (*source*) | Equipo cuyo escritorio se extiende o duplica. Crea el monitor virtual, captura, codifica y transmite. |
| **Receptor** (*sink*) | Equipo que actúa como pantalla. Recibe, decodifica y presenta a pantalla completa. Opcionalmente devuelve entrada (teclado, ratón, táctil). |
| **Monitor virtual** | Pantalla que el sistema operativo de la fuente reconoce como un monitor físico (aparece en *Configuración de pantalla*, admite resolución y frecuencia, se puede extender). |
| **Sesión** | Conexión autenticada entre una fuente y un receptor, que puede contener uno o varios flujos de pantalla. |

Un mismo binario puede actuar como fuente, receptor o ambos. "Bidireccional" significa que cualquier equipo puede asumir cualquier rol, no que haya un solo flujo en ambos sentidos.

## Documentación

| # | Documento | Contenido |
|---|---|---|
| 1 | [Análisis de repositorios de referencia](docs/01-analisis-repositorios.md) | TwinDesk, Deskreen, Weylus, Sunshine, ScreenExtend: auditoría de código, licencias, lo anunciado frente a lo implementado |
| 2 | [Arquitectura técnica](docs/02-arquitectura.md) | Capas, procesos, flujo de datos, presupuesto de latencia, diagrama de componentes |
| 3 | [Monitores virtuales](docs/03-monitores-virtuales.md) | Estrategia para Windows (IddCx) y Linux (Mutter, KWin, wlroots, EVDI, X11) |
| 4 | [Protocolos de streaming](docs/04-protocolos-streaming.md) | WebRTC, RTP/UDP propio, QUIC, SRT, WebSocket, MoQ, RDP; decisión de transporte y protocolo de cable |
| 5 | [Estructura del monorepo](docs/05-estructura-monorepo.md) | Crates, aplicaciones, drivers, herramientas, CI |
| 6 | [Riesgos y limitaciones](docs/06-riesgos.md) | Matriz de riesgos con mitigación |
| 7 | [Roadmap](docs/07-roadmap.md) | Fases con criterios de salida |
| 8 | [Pruebas de concepto](docs/08-pruebas-de-concepto.md) | PoC necesarias para validar la viabilidad, con criterios de éxito medibles |
| 9 | [Contrato de sesión del MVP](docs/09-contrato-sesion-mvp.md) | Protocolo entre emisor y receptor: control, vídeo y cursor |
| 10 | [Prueba entre dos equipos](docs/10-prueba-dos-equipos.md) | Paso a paso para probar Windows → Windows por cable y Wi-Fi |
| 11 | [Linux](docs/11-receptor-linux.md) | Ubuntu como pantalla y como fuente (extender/duplicar), códec, registros |
| — | [Resultados de las PoC](docs/poc-results/) | Mediciones reales de cada prueba |
| — | [Decisiones arquitectónicas (ADR)](docs/adr/README.md) | Registro de decisiones |

## Principios no negociables

1. **No simular.** Ninguna funcionalidad se presenta en la interfaz hasta que funciona. No se muestran estadísticas inventadas ni monitores "virtuales" que en realidad son ventanas.
2. **Licencia limpia.** El proyecto se publica bajo `MIT OR Apache-2.0`. Las cinco referencias analizadas son GPL-3.0 o AGPL-3.0, así que **no se copia, traduce ni parafrasea código de ninguna de ellas** (ver [ADR-0001](docs/adr/0001-licencia.md)).
3. **Seguro por defecto.** Todo el tráfico va cifrado y autenticado, también en la LAN; no hay modo "sin cifrar".
4. **Núcleo independiente del SO.** La lógica de sesión, el protocolo y el transporte no dependen de ningún sistema operativo. Lo específico de cada plataforma vive detrás de *traits*.
