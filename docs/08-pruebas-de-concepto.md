# 8. Pruebas de concepto necesarias

Cada PoC responde a **una** pregunta de viabilidad y tiene un criterio de éxito numérico. El código vive en `tools/poc/<nombre>` y es desechable. El resultado se documenta en `docs/poc-results/<nombre>.md` y, si cambia una decisión, actualiza el ADR correspondiente.

**Hardware de referencia mínimo:**
- un PC Windows 11 con NVIDIA, otro con AMD o Intel;
- un PC Linux con Intel o AMD (Ubuntu 24.04+ GNOME, Fedora KDE, Arch con Sway);
- un AP Wi-Fi 6 y un switch Gigabit;
- una cámara de 240 fps (vale la de un móvil).

| ID | Pregunta | Criticidad | Criterio de éxito | Si falla |
|---|---|---|---|---|
| **PoC-1** | ¿Podemos crear, configurar y destruir monitores virtuales con **VDD** de forma programática y fiable? | Crítica | Crear un monitor 1920×1080@60 y otro 2560×1600@120 en < 3 s; aparece en Configuración; se destruye sin reiniciar; resiste 100 ciclos y la suspensión/reanudación; **documentar qué operaciones requieren elevación** | Evaluar virtual-display-rs con firma propia de pruebas; adelantar la decisión del driver propio (F8) |
| **PoC-2** | ¿Captura DXGI DD el monitor virtual en GPU con baja latencia? ¿Qué pasa en portátiles híbridos? | Crítica | Recibir una textura D3D11 en ≤ 2 ms p95 tras `AcquireNextFrame` a 60 fps; identificar el adaptador del monitor virtual | WGC como primario |
| **PoC-3** | ¿Qué latencia y calidad da la codificación H.264 por hardware con entrada D3D11 (MF HW MFT frente a NVENC directo) y VAAPI en Linux? ¿FFmpeg LGPL o `cros-libva`? | Crítica | ≤ 6 ms p95 por frame a 1080p60 y ≤ 12 ms a 4K60; sin frames B; *intra refresh* disponible en al menos un backend por fabricante; lista de qué parámetros respeta MF en cada fabricante | Priorizar los SDK directos (NVENC, AMF, QSV) sobre MF |
| **PoC-4** | ¿Soporta **quinn** con datagramas 80 Mbps de vídeo sintético con latencia y CPU aceptables? ¿Qué controlador de congestión interfiere menos? | Crítica | Latencia de un solo sentido añadida ≤ 1 ms p95 en Gigabit; CPU ≤ 15 % de un núcleo a 50 Mbps; comparar NewReno, Cubic, BBR y un controlador propio; repetir en Wi-Fi | Plan B: UDP propio con AES-GCM y claves del exportador TLS |
| **PoC-5** | ¿Podemos decodificar por hardware y presentar con `wgpu` sin copias (D3D11VA en Windows, VAAPI con DMA-BUF y Vulkan en Linux)? | Crítica | Decodificación + presentación ≤ 6 ms p95 a 1080p60; sin *tearing* en modo mailbox; documentar si la importación DMA-BUF funciona en Intel y AMD | EGL/GL interop o copia a CPU acotada |
| **PoC-6** | Herramienta **glass-to-glass**: ¿cuál es la latencia real del pipeline completo de PoC-2 a PoC-5? | Crítica | Medición reproducible (±2 ms) con un patrón de reloj en pantalla y una cámara de 240 fps; p95 ≤ 35 ms por cable a 1080p60 | Revisar el presupuesto de §2.4 etapa por etapa |
| **PoC-7** | **Monitores virtuales en Linux:** ¿funcionan Mutter `RecordVirtual`, KWin `stream_virtual_output`, Sway/Hyprland *headless* y EVDI, y con qué versiones mínimas? | Crítica | Por backend: crear un monitor con una resolución arbitraria; recibir frames PipeWire (DMA-BUF si es posible); cambiar la resolución; destruirlo. Matriz: GNOME 46/47/48, Plasma 6.x, Sway 1.9+, Hyprland, X11 + EVDI con Secure Boot activado | Reducir el soporte inicial a los entornos que pasen; documentarlo |
| **PoC-8** | Entrada en Linux: ¿libei por el portal RemoteDesktop permite eventos absolutos sobre el monitor virtual de Mutter y KWin? | Media | Clics en la posición correcta del monitor virtual en GNOME y KDE; un solo diálogo de permiso, con persistencia | uinput opcional |
| **PoC-9** | Seguridad: SPAKE2 + certificado fijado + verificador personalizado de rustls dentro de quinn | Alta | Emparejar con un PIN de 6 dígitos; reconectar sin PIN; rechazar un certificado no fijado; tests de MITM simulado en verde | — (hay alternativas: Noise XX + SAS) |
| **PoC-10** | mDNS con `mdns-sd` en Windows (con y sin Bonjour, y con el respondedor del sistema) y en Linux (con Avahi) | Media | Descubrimiento en < 2 s en los 4 escenarios; coexistencia en el puerto 5353 sin errores | API nativa `DnsServiceRegister` en Windows y Avahi D-Bus en Linux |
| **PoC-11** | Arquitectura de procesos: UI Tauri ⇄ agente ⇄ visor con IPC autenticada; el visor se reinicia sin cortar la sesión | Media | Matar el visor y relanzarlo en < 1 s con IDR; cerrar la UI sin afectar el *streaming* | Fusionar visor y agente |

## Orden recomendado

```
Semana 1-2:  PoC-6 (herramienta de medición, la necesitan todas)  ║  PoC-1 (VDD)  ║  PoC-7 (Linux, la de mayor riesgo)
Semana 2-3:  PoC-2 → PoC-3                                         ║  PoC-4
Semana 3-4:  PoC-5                                                 ║  PoC-9, PoC-10
Semana 4-5:  integración PoC-2..5 en un pipeline mínimo medido con PoC-6; PoC-8, PoC-11
```

**PoC-7 y PoC-1 van primero** porque son las que pueden invalidar la premisa del proyecto (monitor virtual real en ambos SO). Si PoC-7 falla para la mayoría de los entornos Linux, el alcance de la 1.0 debe redefinirse antes de escribir código de producto.
