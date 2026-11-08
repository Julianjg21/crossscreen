# 5. Estructura inicial del monorepo

Herramientas: un **Cargo workspace** para Rust y un **pnpm workspace** para la UI, en un único repositorio git.

```
crossscreen/
├── Cargo.toml                    # [workspace], perfiles, lints comunes, versiones fijadas
├── rust-toolchain.toml           # toolchain estable fijada
├── deny.toml                     # cargo-deny: licencias permitidas, advisories, duplicados
├── package.json / pnpm-workspace.yaml
├── LICENSE-MIT / LICENSE-APACHE
├── README.md
│
├── crates/                       # bibliotecas (sin binarios)
│   ├── cs-core/                  # tipos de dominio, errores, config, reloj. SIN deps de SO
│   ├── cs-protocol/              # mensajes de control (postcard), cabecera de vídeo, versionado
│   ├── cs-security/              # identidad Ed25519/X.509, SPAKE2, trust store, verificador rustls
│   ├── cs-transport/             # quinn, fragmentación, FEC, pacing, stats, control de tasa
│   ├── cs-discovery/             # mDNS (mdns-sd), entrada manual de IP
│   ├── cs-session/               # máquina de estados, reconexión, orquestación del pipeline
│   ├── cs-display/               # trait VirtualDisplayProvider + backends
│   │   └── src/{windows/vdd.rs, linux/{mutter,kwin,wlroots,evdi}.rs, mock.rs}
│   ├── cs-capture/               # trait CaptureSource + backends
│   │   └── src/{windows/{dxgi,wgc}.rs, linux/{pipewire,x11,evdi}.rs}
│   ├── cs-encode/                # trait VideoEncoder + {mf, nvenc, amf, qsv, vaapi, openh264}
│   ├── cs-decode/                # trait VideoDecoder + {mf_d3d11va, vaapi, openh264}
│   ├── cs-render/                # presentación wgpu, overlay de cursor, importación de texturas
│   ├── cs-input/                 # captura local + inyección {sendinput, libei, uinput, xtest}
│   ├── cs-ipc/                   # API local agent⇄ui⇄viewer (named pipe / UDS), autenticada
│   └── cs-testkit/               # fuentes sintéticas, red simulada (pérdida/jitter), fixtures
│
├── apps/
│   ├── agent/                    # bin crossscreen-agent (daemon de usuario)
│   ├── viewer/                   # bin crossscreen-viewer (winit + wgpu)
│   ├── helper/                   # bin crossscreen-helper (privilegiado, API mínima)
│   ├── cli/                      # bin crossscreen (diagnóstico: `doctor`, `pair`, `bench`)
│   └── desktop/                  # Tauri 2 + React + TypeScript
│       ├── src-tauri/            # comandos Tauri finos → cliente cs-ipc (sin lógica de negocio)
│       └── src/                  # React: dispositivos, emparejamiento, ajustes, estadísticas
│
├── drivers/
│   ├── windows/                  # manifiesto de VDD (versión, SHA-256, sujeto de firma); F8: IddCx propio
│   └── linux/                    # reglas udev, plantilla polkit (acciones concretas, con auth), notas EVDI
│
├── tools/
│   ├── latency/                  # cs-latency: medición glass-to-glass (patrón con timestamp + cámara)
│   ├── netsim/                   # perfiles de red (tc netem / clumsy) para pruebas
│   └── poc/                      # PoCs aisladas, desechables (ver 08-pruebas-de-concepto.md)
│
├── packaging/                    # MSI/MSIX (WiX), .deb, .rpm, Flatpak (evaluar), AppImage
├── docs/                         # esta documentación + adr/
└── .github/workflows/            # CI
```

## 5.1 Por qué esta división

- **Un crate por responsabilidad, no por SO.** `cs-capture` contiene `windows/` y `linux/` detrás de `#[cfg(target_os)]`. Crear `cs-capture-windows` y `cs-capture-linux` duplicaría *traits* y *wiring* sin ganar aislamiento real, porque Cargo ya compila solo el código de la plataforma objetivo.
- **`cs-core` y `cs-protocol` no tienen dependencias de SO.** Así son testeables en cualquier máquina y en CI barata.
- **`apps/desktop/src-tauri` es una capa fina.** Los comandos Tauri solo traducen a llamadas `cs-ipc`. La lógica vive en el agente, de modo que la UI se puede sustituir (o usar el CLI) sin tocar el núcleo.
- **El helper es un binario aparte**, con superficie mínima y auditable. Es el único que se ejecuta con privilegios.
- **`cs-testkit`** centraliza los dobles de prueba. Los *mocks* se usan **solo en tests**; nunca se compilan en la build de release (`#[cfg(test)]` o una *feature* `testkit` que no esté por defecto).

## 5.2 Dependencias previstas (todas permisivas; se verificarán con `cargo-deny`)

| Área | Crate | Licencia |
|---|---|---|
| Async | `tokio` | MIT |
| QUIC / TLS | `quinn`, `rustls`, `rcgen` | MIT/Apache |
| PAKE | `spake2` (RustCrypto) | MIT/Apache |
| Criptografía | `ed25519-dalek`, `ring` o `aws-lc-rs` | BSD/ISC/Apache |
| mDNS | `mdns-sd` | MIT/Apache |
| Serialización | `serde`, `postcard` | MIT/Apache |
| FEC | `reed-solomon-simd` | MIT |
| Windows | `windows` (windows-rs), `windows-capture` | MIT/Apache, MIT |
| Linux | `ashpd`, `pipewire`, `zbus`, `wayland-client`, `evdev`, `reis` (libei) | MIT |
| Render | `wgpu`, `winit` | MIT/Apache, Apache |
| Software H.264 | `openh264` (binario de Cisco descargado en tiempo de ejecución) | BSD-2 |
| UI | `tauri` 2, React, TypeScript | MIT/Apache |
| Logs | `tracing` | MIT |

`deny.toml` **rechaza** GPL, AGPL, LGPL enlazado de forma estática y licencias desconocidas. La única excepción permitida es LGPL enlazada **dinámicamente** (FFmpeg LGPL, `libevdi`), y queda documentada en la lista de excepciones.

## 5.3 Estrategia de pruebas

| Nivel | Qué | Dónde |
|---|---|---|
| Unitarias | Serialización del protocolo (incluido *round-trip* con `proptest`), FEC (recuperación con pérdidas aleatorias), máquina de estados de sesión, verificador de certificados, SPAKE2 | Todas las plataformas en CI |
| *Fuzzing* | Parser de cabecera de vídeo y de mensajes de control (`cargo-fuzz`) | Nightly en CI |
| Integración | Fuente sintética → codificador software → transporte en loopback con `netsim` (pérdida 0–10 %, *jitter*) → decodificador → comprobación de píxeles por PSNR | Linux y Windows en CI |
| Plataforma | Creación y destrucción de monitores (VDD en un *runner* Windows con el driver; `vkms`/headless Sway en Linux), captura y codificación por hardware | *Runners self-hosted* con GPU (fase 2+) |
| Extremo a extremo | `cs-latency` con cámara de 240 fps; lista de comprobación manual de modos | Laboratorio, en cada release |
| Seguridad | Tests negativos: certificado no fijado, PIN incorrecto, repetición, bloqueo por intentos | CI |

## 5.4 CI (GitHub Actions)

- **Matriz:** `windows-latest`, `ubuntu-24.04` y `ubuntu-24.04-arm` (compilación).
- **Pasos:**
  1. `cargo fmt --check`
  2. `cargo clippy -D warnings`
  3. `cargo test`
  4. `cargo deny check`
  5. `pnpm lint`, `pnpm typecheck` y `pnpm test` (Vitest)
- **Builds de Tauri** en *tags*. Firma de código (Authenticode) en una fase posterior.
- **Puerta de rendimiento** (inspirada en ScreenExtend): benchmark del pipeline software en loopback con un umbral de regresión.
