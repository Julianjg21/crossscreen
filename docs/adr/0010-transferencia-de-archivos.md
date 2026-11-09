# ADR-0010: Transferencia de archivos por streams QUIC, opcional en los dos lados

- **Estado:** Aceptada (2026-11-09)
- **Contexto:** F7 (funcionalidades de valor añadido).

## Contexto

Quien usa el otro equipo como pantalla quiere pasar archivos entre los dos sin otra herramienta: una foto, un documento o un instalador. Requisito del dueño: **por defecto solo se transmite la imagen**. Los archivos se activan a propósito en el emisor y en el receptor.

## Decisión

1. **Transporte:** cada archivo va en su propio **stream QUIC unidireccional** de la conexión de la sesión. Es fiable, va cifrado y autenticado con la misma huella fijada, y no interfiere con el canal de control. Formato:

   ```text
   "CSF1" | u16 LE longitud del nombre | nombre | u64 LE tamaño | bytes
   ```

   El emisor cierra el stream tras el último byte. Máximo 16 GiB por archivo. Funciona en los dos sentidos.
2. **Negociación:** cada lado con la función activada envía `FilesAvailable { enabled: true }`.
   - Solo se abren streams de archivo hacia un par que lo haya anunciado. Si no lo anuncia en 5 s, el envío falla con un mensaje claro.
   - Un lado sin la función activada no acepta streams de archivo.
   - Pares antiguos ignoran el mensaje: no hay transferencia y nada falla. Protocolo 0.5.
3. **Velocidad:** el envío se limita a **6 MB/s (~48 Mbit/s)** para no ahogar el vídeo, que va por datagramas en la misma conexión. Los archivos de una cola salen de uno en uno, en orden.
4. **Recepción segura:**
   - **Nombre saneado:** solo el último componente, sin `..`, sin caracteres de control ni prohibidos en Windows, sin nombres reservados (`CON`, `NUL`…), máximo 180 caracteres.
   - **Escritura atómica:** se escribe en `.nombre.part`. Solo si llega exactamente el tamaño anunciado se renombra; si falta o sobra, se borra.
   - **Nunca se sobrescribe:** `foto (1).png`, `foto (2).png`…
   - **Destino fijo:** `Descargas/CrossScreen` (o `CROSSSCREEN_FILES_DIR`). En Linux se respeta `XDG_DOWNLOAD_DIR`.
   - **Nunca se abre nada solo.** En Windows el archivo recibido se marca como descargado de Internet (*Mark of the Web*), y el sistema avisa antes de ejecutarlo.
5. **Interfaz:**
   - **Emisor (panel):** casilla «Permitir enviar y recibir archivos». Durante la transmisión, los archivos se arrastran a la ventana del panel. El panel lista los enviados y recibidos y tiene un botón para abrir la carpeta. Por IPC 1.7: `StartStream.files`, `SendFiles` y eventos `FileTransfer`.
   - **Receptor:** casilla equivalente (`--files` en el visor). Los archivos se sueltan sobre la ventana del visor.
   - **CLI:** `crossscreen stream --files` y `--send-file RUTA`.
   - **Varios monitores:** los archivos van solo con el primero, como el portapapeles y el audio.
6. **Reconexión:** la cola de salida sobrevive y el servicio se reanuda sobre la conexión nueva. Un archivo que estuviera a medias se pierde (no se reanuda) y se informa como fallido.

## Consecuencias

- **A favor:**
  - sin puertos ni servicios nuevos;
  - el mismo cifrado y la misma autenticación que la sesión;
  - poco código, con pruebas sobre QUIC real (ida y vuelta, nombres hostiles, cuerpos incompletos, carrera al anunciar).
- **En contra:**
  - no se reanudan transferencias cortadas;
  - no hay carpetas (hay que comprimirlas antes);
  - la velocidad está limitada aunque la red dé para más.
  - En Linux con Wayland, winit aún no entrega «soltar archivo» al visor: allí se reciben archivos, pero se envían desde el panel del emisor.
- **Revisable:**
  - ajustar el límite de velocidad según la tasa de vídeo y el RTT;
  - reanudar por desplazamiento;
  - carpetas.
