# ADR-0008: Audio del sistema con Opus en Rust puro, opcional y negociado

- **Estado:** Aceptada (2026-11-09)
- **Contexto:** F7 (funcionalidades de valor añadido).

## Contexto

Al usar el otro equipo como pantalla, el sonido de lo que se muestra (vídeo, llamadas, avisos) debe poder oírse en el receptor. Requisitos del dueño: **por defecto solo se transmite la imagen**; el audio se activa a propósito, en el emisor y en el receptor.

## Decisión

1. **Captura:** el audio que reproduce el emisor.
   - Windows: WASAPI en modo *loopback* sobre el dispositivo de salida por defecto.
   - Linux: PipeWire, flujo de captura del *monitor* del sumidero por defecto (`stream.capture.sink = true`).
2. **Formato y códec:** 48 kHz estéreo en coma flotante, **Opus** en tramas de **10 ms** (modo `Audio`, ~128 kbps), con remuestreo si el dispositivo trabaja a otra frecuencia.
   - Implementación: crate **`opus1`** (Rust puro, BSD-3-Clause). Se descartó `libopus` vía `opus`/`audiopus_sys`: obliga a compilar C con CMake (falla con CMake 4 y con el generador de Visual Studio) y enlaza el CRT dinámico, incompatible con el CRT estático del proyecto.
   - Verificado: los paquetes de `opus1` y de `libopus` 1.x se decodifican entre sí con la misma calidad (SNR 43,4 dB en las cuatro combinaciones, misma tasa) y `opus1` codifica algo más rápido (0,11 ms frente a 0,13 ms por 20 ms estéreo).
3. **Transporte:** datagramas QUIC propios (primer byte `2`; el vídeo usa `1`), sin fragmentar ni FEC: una trama Opus cabe en un datagrama y Opus tolera pérdidas (ocultación en el decodificador). Cabecera: tipo, flujo, número de secuencia y marca de tiempo en muestras de 48 kHz.
4. **Negociación (opt-in en ambos lados):**
   - el receptor, si tiene el audio activado, envía `AudioRequest { enabled: true }`;
   - el emisor, solo si **también** lo tiene activado, captura y anuncia `AudioStart` antes de enviar datagramas, y `AudioStop` al parar.
   - Pares antiguos ignoran estos mensajes desconocidos: no se envía audio y nada falla. Protocolo 0.4.
5. **Reproducción:** búfer de *jitter* adaptativo (objetivo ~60 ms) que entrega bloques fijos al dispositivo; ante pérdidas, ocultación de Opus; ante vacío, silencio.

## Consecuencias

- Sin dependencias de C ni de CMake para el audio, en Windows y en Linux.
- La sincronía con el vídeo es aproximada (ambos caminos tienen latencias de decenas de ms); se ajustará con mediciones.
- El tráfico extra (~130 kbps) solo existe si los dos lados lo activan.
