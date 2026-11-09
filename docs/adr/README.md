# Registro de decisiones arquitectónicas (ADR)

Formato: contexto → decisión → alternativas → consecuencias.
Estados posibles: *Propuesta*, *Aceptada*, *Reemplazada por ADR-XXXX*.

Un ADR aceptado no se edita: si la decisión cambia, se escribe uno nuevo que lo reemplace.

| ADR | Título | Estado |
|---|---|---|
| [0001](0001-licencia.md) | Licencia `MIT OR Apache-2.0` y política de no reutilizar código copyleft | Propuesta |
| [0002](0002-transporte-quic.md) | Transporte QUIC (quinn) con datagramas para vídeo | Propuesta, validar con la PoC-4 |
| [0003](0003-receptor-nativo.md) | Receptor nativo (winit + wgpu) separado de la UI Tauri | Propuesta, validar con las PoC-5 y PoC-11 |
| [0004](0004-seguridad-emparejamiento.md) | Emparejamiento SPAKE2 y TLS mutuo con certificados fijados | Propuesta, validar con la PoC-9 |
| [0005](0005-codificacion-sin-ffmpeg-gpl.md) | Codificación con APIs nativas y SDK de fabricante; FFmpeg solo LGPL y dinámico | Propuesta, validar con la PoC-3 |
| [0006](0006-monitor-virtual-windows.md) | Monitor virtual en Windows: VDD externo ahora, IddCx propio después | Propuesta, validar con la PoC-1 |
| [0007](0007-monitor-virtual-linux.md) | Monitor virtual en Linux: backends por compositor con EVDI de respaldo | Propuesta, validar con la PoC-7 |
| [0008](0008-audio.md) | Audio del sistema con Opus en Rust puro, opcional y negociado | Aceptada |
| [0009](0009-varios-monitores.md) | Varios monitores por sesión como sesiones paralelas | Aceptada |
| [0010](0010-transferencia-de-archivos.md) | Transferencia de archivos por streams QUIC, opcional en los dos lados | Aceptada |
