# ADR-0002: Transporte QUIC (quinn) con datagramas para vídeo

**Estado:** Propuesta (validar con la PoC-4) · **Fecha:** 2026-11-08

## Contexto
Necesitamos:
- vídeo no fiable sin bloqueo de cabeza de línea;
- control y entrada fiables;
- cifrado y autenticación mutua;
- control total del *pacing* y del códec;
- robustez en Wi-Fi.

Los dos extremos son aplicaciones nativas en Rust, de modo que no hace falta compatibilidad con navegadores. Análisis completo en [04-protocolos-streaming.md](../04-protocolos-streaming.md).

## Decisión
Usar una conexión **QUIC** por sesión, con `quinn` y `rustls`:
- **vídeo, cursor y audio** como datagramas (RFC 9221), con FEC Reed-Solomon y *pacing* propios;
- **control, entrada y archivos** en streams.

## Alternativas descartadas
- **WebRTC (libwebrtc, webrtc-rs, str0m).** Su complejidad (ICE, SDP, DTLS, SCTP) solo aporta valor con navegadores o NAT, que no forman parte del caso de uso. Las referencias muestran sus costes: el *packetizer* y el *pacer* que ScreenExtend tuvo que reescribir, y los *field trials* de los que depende TwinDesk. str0m queda como plan B si se añade un receptor web.
- **RTP/UDP propio + ENet (estilo Sunshine).** Obliga a diseñar criptografía propia (Sunshine lo hace con AES-ECB en el emparejamiento y con el vídeo sin cifrar en LAN por defecto).
- **SRT.** Su ventana de latencia fija no encaja con contenido interactivo y no autentica a los pares.
- **TCP/WebSocket.** Bloqueo de cabeza de línea.

## Consecuencias
- TLS 1.3 auditado y migración de conexión incluidos sin coste adicional.
- Los datagramas QUIC están sujetos al control de congestión de QUIC. Hay que medirlo y, si hace falta, escribir un controlador propio.
- Si la PoC-4 demuestra un coste de CPU inaceptable: el vídeo pasa a UDP propio, con AES-GCM y claves derivadas del exportador TLS de la conexión QUIC (RFC 5705), y el control se queda en QUIC.
