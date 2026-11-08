# 7. Roadmap de implementación por fases

Cada fase tiene **criterios de salida medibles**. No se empieza la siguiente fase hasta cumplirlos. Las duraciones suponen 1–2 desarrolladores a tiempo completo y son orientativas.

```mermaid
gantt
  dateFormat  YYYY-MM-DD
  axisFormat  %b
  section Fundamentos
  F0 Fundamentos            :f0, 2026-11-16, 3w
  F1 Pruebas de concepto    :f1, after f0, 5w
  section Producto
  F2 MVP duplicar Win→Win   :f2, after f1, 6w
  F3 Extender Windows (VDD) :f3, after f2, 4w
  F4 Linux receptor y fuente:f4, after f3, 8w
  F5 Robustez y Wi-Fi       :f5, after f4, 4w
  section Extensiones
  F6 Entrada / KVM          :f6, after f5, 4w
  F7 Portapapeles, audio, archivos, multi-monitor :f7, after f6, 8w
  F8 Driver propio, HEVC/AV1, HDR :f8, after f7, 10w
```

---

## Estado a 2026-11-08

| Fase | Estado |
|---|---|
| F0 | ✅ En lo esencial: workspace, CI (configurado; aún sin ejecutar en GitHub), `cargo-deny` en verde, licencias verificadas. Pendiente: `cs-testkit`/`netsim` y *fuzzing* con `cargo-fuzz` |
| F1 | 🟡 Windows hecho: PoC-1, 2, 3 y 5 completas; PoC-4 solo en loopback. Pendientes: PoC-6 (cámara), PoC-7 y 8 (Linux), PoC-9/10/11 parcialmente cubiertas por los tests y el panel |
| F2 | 🟡 Cadena completa funcionando en un mismo equipo (captura → `Present()` p50 29 / p95 32 ms, 0 pérdidas). Faltan: prueba entre dos PCs por cable y Wi-Fi, estabilidad de 2 h, integración en agente/panel, y reducir `Present()`→*vblank* (~33 ms en ventana) |
| F3 | 🟡 Adelantada: el monitor virtual con VDD y la topología CCD ya funcionan (PoC-1), aunque falta integrarlos en el agente |
| F4–F8 | ⏳ No iniciadas |

**Riesgo nuevo para F2:** en este portátil (Intel Iris Xe) la codificación MF no alcanza el presupuesto de latencia. Hay que medir en hardware con NVENC/AMF o probar oneVPL antes de dar por cerrado el objetivo de ≤ 40 ms *glass-to-glass*.

### F0: Fundamentos (≈ 3 semanas)
- Repositorio con la estructura de [05-estructura-monorepo.md](05-estructura-monorepo.md), CI y `cargo-deny`.
- Verificar las licencias upstream: VDD, virtual-display-rs, windows-capture y los crates de la tabla de dependencias.
- `cs-core` y `cs-protocol` con tests de *round-trip* y *fuzzing* básico.
- `cs-testkit` con una fuente sintética y `netsim`.
- **Salida:**
  - CI en verde en Windows y Linux;
  - ADR-0001 a ADR-0005 aprobados;
  - licencias verificadas y documentadas.

### F1: Pruebas de concepto (≈ 5 semanas)
- Ejecutar las PoC de [08-pruebas-de-concepto.md](08-pruebas-de-concepto.md). Son código desechable en `tools/poc/`.
- **Salida:**
  - las PoC críticas (1, 3, 4, 5, 6, 7) con resultados documentados;
  - decisiones confirmadas o revisadas en los ADR;
  - el presupuesto de latencia de §2.4 contrastado con mediciones reales.

### F2: MVP "duplicar" Windows → Windows por cable (≈ 6 semanas)
- Captura DXGI DD de un monitor existente → codificación MF HW → QUIC con FEC → decodificación D3D11VA → visor `wgpu`.
- Emparejamiento SPAKE2, certificados fijados y descubrimiento mDNS.
- Cursor fuera de banda.
- UI Tauri mínima: lista de equipos, emparejar, conectar, estadísticas **reales**.
- **Salida:**
  - 1080p60 estable durante 2 h;
  - p95 glass-to-glass ≤ 40 ms por Ethernet;
  - reconexión tras un corte de cable de 5 s;
  - tests de seguridad negativos en verde.

### F3: Extender en Windows con monitor virtual (≈ 4 semanas)
- Backend `cs-display::windows::vdd`, instalación verificada a través del helper.
- Topología CCD y restauración al terminar.
- Selección de resolución y frecuencia según el receptor.
- **Salida:**
  - el monitor aparece en Configuración, se pueden arrastrar ventanas a él y desaparece limpiamente;
  - sobrevive a la suspensión y la reanudación;
  - el driver no queda en un estado inconsistente tras matar el agente (prueba de *kill*).

### F4: Linux (≈ 8 semanas)
- **F4a, receptor Linux** (Windows → Linux): decodificación VAAPI con OpenH264 de respaldo, visor `wgpu` en Wayland y X11.
- **F4b, fuente Linux en duplicar:** portal y PipeWire con `restore_token`; X11 con XShm.
- **F4c, fuente Linux en extender:** backends `mutter-virtual`, `kwin-virtual` y `wlroots-headless`, y luego `evdi`. Codificación VAAPI.
- **Salida:** las cuatro combinaciones (W→W, W→L, L→W, L→L) en duplicar y en extender, en GNOME 46+, Plasma 6 y Sway, con los mismos umbrales que F2.

### F5: Robustez, Wi-Fi y sesiones (≈ 4 semanas)
- Control de tasa adaptativo y FEC adaptativo, perfiles de conexión (cable, Wi-Fi, ahorro).
- Migración de interfaz.
- Ventana de gracia para conservar el monitor.
- Telemetría local de latencia, FPS, bitrate y pérdidas.
- **Salida:**
  - p95 ≤ 60 ms en Wi-Fi 6 a 5 m del AP;
  - sin congelaciones de más de 1 s con un 5 % de pérdida en `netsim`.

### F6: Entrada remota y KVM (≈ 4 semanas)
- Captura de entrada local en el visor; inyección con SendInput y libei, uinput y XTest de respaldo.
- Táctil y pluma.
- **Salida:** latencia de ratón ≤ 1 RTT + 2 ms; ningún evento perdido ni repetido en 10 000 eventos.

### F7: Funcionalidades de valor añadido (≈ 8 semanas)
- Portapapeles bidireccional (texto e imágenes).
- Audio Opus (WASAPI loopback / PipeWire).
- Transferencia de archivos por streams QUIC.
- Varios monitores virtuales por sesión.
- Perfiles.
- **Salida:** cada funcionalidad con tests de integración y aparece en la UI solo cuando está completa.

### F8: Avanzado (≈ 10 semanas)
- Driver IddCx propio (atestación de Microsoft, acceso a la *swap chain*, selección de adaptador).
- HEVC y AV1.
- HDR.
- Receptor web opcional (WebTransport + WebCodecs).
- **Salida:** según el ADR correspondiente en el momento.

---

## Hitos de publicación

| Versión | Contenido | Estado público |
|---|---|---|
| 0.1 | F2 | Alfa privada |
| 0.2 | F3 | Alfa pública solo para Windows |
| 0.3 | F4 | Beta multiplataforma |
| 0.5 | F5 + F6 | Beta |
| 1.0 | F7, auditoría de seguridad externa, firma de código | Estable |
