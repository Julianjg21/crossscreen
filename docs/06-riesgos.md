# 6. Riesgos técnicos y limitaciones

Escala: Probabilidad (P) e Impacto (I) de 1 a 5. Severidad = P × I.

## 6.1 Matriz de riesgos

| # | Riesgo | P | I | Sev. | Mitigación | Validación |
|---|---|---|---|---|---|---|
| R1 | **Monitor virtual en Linux fragmentado:** cada compositor tiene su API, las APIs de Mutter y KWin son privadas o inestables, y X11 depende de EVDI (DKMS y Secure Boot) | 5 | 5 | **25** | Varios backends con prioridad y diagnóstico explícito; matriz de versiones probadas; EVDI como respaldo; el modo duplicar siempre disponible y presentado como tal | PoC-7 |
| R2 | **Firma del driver en Windows:** dependemos de VDD (terceros, firma SignPath); un driver propio exige certificado EV, atestación de Microsoft y coste anual; ARM64 no funciona sin firma de Microsoft | 4 | 5 | **20** | Fases 1–3 con VDD fijado por versión y hash; monitorizar upstream; presupuestar el certificado EV antes de la fase 8; ARM64 queda fuera del alcance inicial | PoC-1 ✅ (VDD es viable por la vía helper + CCD) |
| R2b | **Calidad de VDD** (PoC-1): sus comandos de tubería tumban el host UMDF (código 43 tras 3 caídas), y la tubería está abierta a Everyone (DoS local o por SMB) | 5 | 3 | 15 | No usar la tubería; reconfigurar con helper + reinicio del dispositivo; detectar el código 43 y ofrecer recuperación; adelantar el driver propio (F8) | PoC-1 |
| R3 | **Latencia en Wi-Fi:** pérdidas en ráfaga, ahorro de energía del adaptador, *jitter* de 5–50 ms | 5 | 4 | **20** | FEC adaptativo, *pacing*, control de tasa, *intra refresh*; aviso en la UI cuando la red no da la calidad; recomendar 5 GHz o 6 GHz o cable | PoC-4, PoC-6 |
| R4 | **Diferencias de codificadores y drivers de GPU:** MF HW MFT se comporta distinto según el fabricante (latencia, VBV, *intra refresh* no siempre disponibles); fallos o bloqueos de NVENC/AMF | 4 | 4 | 16 | Sondeo de capacidades al arrancar; detector de bloqueo con caída a otro backend; matriz de pruebas con NVIDIA, AMD e Intel | PoC-3 |
| R5 | **Rendimiento de quinn** a 50–80 Mbps con datagramas cifrados, y control de congestión que añade latencia | 3 | 4 | 12 | Medir pronto; GSO; ajustar el controlador; plan B con UDP propio y claves exportadas de TLS | PoC-4 |
| R6 | **Zero-copy en Linux:** importar DMA-BUF de VAAPI en `wgpu` (Vulkan) depende de la versión de `wgpu` y de los drivers | 3 | 3 | 9 | Ruta EGL/GL o copia acotada (1080p cabe holgado en el presupuesto) | PoC-5 |
| R7 | **Escritorio seguro de Windows** (UAC, bloqueo) no capturable desde la sesión del usuario | 5 | 2 | 10 | Aviso en el receptor; fuera del alcance del MVP; más adelante, un servicio con captura en `winlogon` | — |
| R8 | **Portátiles híbridos:** el monitor virtual se renderiza en la iGPU y el codificador está en la dGPU | 3 | 3 | 9 | Detectar el adaptador del monitor y usar un codificador del mismo; driver propio con selección de adaptador en la fase 8 | PoC-2 |
| R9 | **mDNS en Windows:** conflicto en el puerto 5353 con el respondedor del sistema o con Bonjour; firewalls corporativos que bloquean *multicast*; AP con aislamiento de clientes | 4 | 2 | 8 | `SO_REUSEADDR`; evaluar `DnsServiceRegister` nativo como alternativa; entrada manual de IP y código QR siempre disponibles | PoC-10 |
| R10 | **Entrada en Wayland:** sin libei y portal, solo queda uinput (requiere el grupo `input`, un riesgo de seguridad) | 3 | 2 | 6 | libei vía portal RemoteDesktop como vía principal; uinput opcional y explicado. En modo extender no se necesita | PoC-8 |
| R11 | **Patentes de códecs** (H.264 y HEVC) al distribuir codificadores | 2 | 3 | 6 | Usar los codificadores del SO o de la GPU (licenciados por el fabricante) y el binario OpenH264 de Cisco; no incluir x264 ni x265 | Revisión legal antes de la 1.0 |
| R12 | **Contaminación de licencia** por copiar código de las referencias (todas GPL/AGPL) | 2 | 5 | 10 | ADR-0001; `cargo-deny`; revisión en PR; política escrita para contribuyentes | Continua |
| R13 | **Superficie privilegiada:** helper, driver y reglas udev | 3 | 4 | 12 | Helper mínimo bajo demanda con argumentos validados; nada de polkit sin contraseña ni tareas como SYSTEM; auditoría externa antes de la 1.0 | Revisión de seguridad |
| R14 | **Alcance excesivo:** la lista de funcionalidades futuras equivale a TwinDesk entero | 4 | 4 | 16 | Roadmap por fases con criterios de salida; no empezar audio, archivos ni KVM antes de estabilizar el modo extender | Revisión por fase |
| R15 | **Cambios de API del SO:** actualizaciones de Mutter, KWin o Windows que rompan backends | 4 | 3 | 12 | Tests de plataforma en CI con versiones fijadas; detección de versión con mensaje claro | Continua |
| R16 | **HDR, color y escalado:** mapeo de tonos, perfiles ICC, DPI fraccionario | 3 | 2 | 6 | MVP en SDR sRGB; HDR en la fase 8 | — |

## 6.2 Limitaciones asumidas (no son defectos; se documentan en la UI)

- **Latencia mínima.** Siempre habrá ≥ 1 frame de latencia añadida respecto a un monitor físico, así que no es apto para juegos competitivos en el monitor extendido.
- **Escritorio seguro.** La pantalla de UAC y la de bloqueo de Windows no se ven en el monitor remoto (MVP).
- **Linux sin compositor soportado ni EVDI:** solo se ofrece el modo duplicar.
- **Windows on ARM:** no soportado como fuente mientras no exista un driver firmado por Microsoft.
- **Un receptor por monitor virtual.** Varios receptores sobre el mismo monitor (difusión) queda fuera del alcance.
- **Solo LAN.** No hay NAT traversal ni *relay*. El uso por Internet queda fuera del alcance (no hay STUN/TURN).
- **Contenido protegido.** HDCP y DRM (por ejemplo, Netflix) aparecerán en negro en el monitor virtual. Es una restricción del SO, no un error.
