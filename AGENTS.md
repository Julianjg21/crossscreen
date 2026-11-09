# AGENTS.md: instrucciones para cualquier agente que trabaje en CrossScreen

> **Lee primero [`docs/ESTADO.md`](docs/ESTADO.md):** qué está hecho, qué está en curso, qué sigue y cómo está montado el entorno de prueba.
> **Al terminar una sesión de trabajo, actualiza `docs/ESTADO.md`** (sección "Bitácora" y lo que haya cambiado). Es la memoria compartida entre agentes y sesiones.

## Qué es
CrossScreen convierte un segundo PC en un **monitor adicional real** (monitor virtual reconocido por el SO) por Ethernet o Wi-Fi, entre Windows y Linux en ambos sentidos.
- Núcleo en Rust; panel con Tauri + React + TypeScript.
- Visión general en `README.md`; arquitectura en `docs/02-arquitectura.md`; decisiones en `docs/adr/`.

## Reglas obligatorias
1. **Sin atribución de IA.** Nunca añadas `Co-Authored-By: Claude…`, "Generated with Claude Code" ni ninguna atribución en commits, PRs, tags, releases, código o docs. El dueño del repo lo pidió explícitamente.
2. **Licencia `MIT OR Apache-2.0`.**
   - **Prohibido copiar, traducir o parafrasear código** de Sunshine, TwinDesk, ScreenExtend, Weylus o Deskreen (todos GPL/AGPL; ver ADR-0001).
   - Las dependencias deben ser MIT/Apache/BSD/ISC/Zlib. LGPL solo enlazada dinámicamente y documentada en `deny.toml`.
   - `cargo deny check` debe pasar.
3. **No simular.** Nada de datos falsos, estadísticas inventadas ni funciones "de mentira" en la UI. Reporta las cifras medidas, incluidos los fallos. Lo que no se ha probado se declara como no probado.
4. **Idioma:** comentarios, docs, mensajes de UI y de commit en **español**; identificadores de código en inglés.
5. **Calidad antes de entregar:**
   - `cargo fmt --all --check`
   - `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`
   - `cargo test --workspace`
   - Si tocas el panel: `npm run build` en `apps/desktop`.
6. **`unsafe` solo en módulos FFI**, con `#![allow(unsafe_code)]` y un comentario `// SAFETY:` en cada bloque. El workspace lo prohíbe por defecto. Usa `windows = "0.58"`.
7. **Seguridad:** todo el tráfico va cifrado y autenticado (QUIC/TLS 1.3 con huellas fijadas y emparejamiento SPAKE2). No añadas modos sin cifrar ni canales locales accesibles a otros usuarios.

## Precauciones en este equipo (Windows del dueño)
- **Nunca envíes comandos a la tubería `\\.\pipe\MTTVirtualDisplayPipe`** del driver VDD: lo tumba (código 43). Ver `docs/poc-results/poc-01-vdd.md`.
- **El monitor virtual:** si lo muestras (`crossscreen display show` o `crossscreen stream`), **déjalo retirado al terminar** (`crossscreen display hide`). Si no, el ratón se pierde en una pantalla invisible.
- **No ejecutes nada que requiera administrador** sin que el dueño lo pida; el helper (`crossscreen-helper`) pide UAC.
- **No mates procesos `node.exe`:** VS Code y Claude Code corren sobre Node.
- **Si la sesión de Windows está bloqueada**, cambiar la topología falla con `SessionNotInteractive`. Es esperado.
- **Varios agentes a la vez:** usa un `CARGO_TARGET_DIR=target-<nombre>` propio para no bloquear el `target/` común. Ya están ignorados por git.

## Git y GitHub
- Repo: <https://github.com/Julianjg21/crossscreen>, rama `main`. El dueño autoriza a los agentes a hacer commit y push.
- Commits pequeños con mensaje en español que explique el porqué. **Sin coautor.**
- **Release:** `git tag -a v0.0.1-alpha.N -m "…" && git push origin v0.0.1-alpha.N`. GitHub Actions compila en Windows y publica el `.zip` en Releases (`.github/workflows/release.yml`).
- El CI (`.github/workflows/ci.yml`) cubre Windows (workspace completo), Linux (crates portables) y `cargo-deny`. Los tests que necesitan GPU se omiten solos en CI.
- **Si trabajas en paralelo con otros agentes**, haz commit solo de tus archivos (`git add <rutas>`), no `git add -A`.

## Mapa rápido del código
| Ruta | Qué es |
|---|---|
| `crates/cs-core` | Tipos de dominio (modos, códecs, ids) |
| `crates/cs-protocol` | Mensajes de control y cabecera de vídeo de 24 B |
| `crates/cs-security` | Identidad (DPAPI en Windows), SPAKE2, verificadores TLS, almacén de confianza |
| `crates/cs-transport` | QUIC (quinn), *packetizer* + reensamblador con FEC, *pacer*, emparejamiento |
| `crates/cs-discovery` | mDNS `_crossscreen._udp` |
| `crates/cs-session` | Máquina de estados de sesión |
| `crates/cs-display` | Monitor virtual: topología CCD (Windows) |
| `crates/cs-capture` | Captura DXGI Desktop Duplication (Windows) |
| `crates/cs-encode` | H.264 por hardware, Media Foundation (Windows) |
| `crates/cs-decode`, `crates/cs-render` | Decodificación MF y presentación D3D11 sin copias (Windows) |
| `crates/cs-pipeline` | Emisor completo reutilizable (`run_sender`) |
| `crates/cs-ipc` | IPC agente ↔ panel (named pipe restringida / socket Unix) |
| `apps/cli` | `crossscreen`: `id`, `pair-listen`, `pair`, `peers`, `forget`, `discover`, `display`, `stream`, `bench`, `bench-serve` |
| `apps/viewer` | `crossscreen-viewer`: receptor (`--listen`) y reproductor de archivos |
| `apps/helper` | `crossscreen-helper`: instala o desinstala el driver VDD (admin) |
| `apps/agent` | `crossscreen-agent`: demonio de usuario |
| `apps/desktop` | Panel Tauri |
| `docs/poc-results/` | Mediciones reales de cada prueba |

## Decisión del dueño: funciones opcionales
Por defecto **solo se transmite la imagen**. Todo lo demás (control remoto, portapapeles, audio, transferencia de archivos y lo que venga) debe poder activarse y desactivarse, y empieza **desactivado**.

