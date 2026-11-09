# ADR-0009: Varios monitores por sesión como sesiones paralelas

- **Estado:** Aceptada (2026-11-09)
- **Contexto:** F7 (funcionalidades de valor añadido).

## Contexto

Un mismo emisor puede querer extender su escritorio con **varios** monitores a la vez, por ejemplo un portátil que usa las dos pantallas de un sobremesa. Requisito del dueño: **por defecto, un solo monitor**; los demás se piden a propósito.

Hasta ahora toda la cadena trabaja con un único flujo:

- el emisor crea un monitor virtual, lo captura y lo codifica;
- el protocolo tiene un `STREAM_ID` fijo;
- el visor abre una sola ventana con un decodificador.

Para meter varios flujos en una conexión habría que rehacer:

- el visor: varias ventanas y decodificadores en un mismo bucle de eventos;
- el control: claves de fotograma, estadísticas y cursor por flujo;
- la reconexión.

## Decisión

1. **Un monitor = una sesión independiente.** Cada monitor tiene:
   - su monitor virtual;
   - su conexión QUIC (con la misma huella fijada);
   - su visor en el receptor.

   Se reutiliza sin cambios todo el camino de un monitor: FEC, *pacing*, cursor, entrada remota, reconexión y el resumen de métricas.
2. **Puertos consecutivos.** El monitor `i` (desde 0) se conecta al puerto base del receptor más `i`. El receptor abre un visor por pantalla en `puerto`, `puerto + 1`… Con varias pantallas, el visor `i` va al monitor físico `i`. Si hay menos monitores físicos, va en ventana al principal.
3. **Lo que es del equipo, solo con el primero.** El portapapeles y el audio del sistema viajan solo con el monitor 1; con varios habría ecos y sonido duplicado. La entrada remota va con cada monitor: cada visor controla el suyo.
4. **Monitores virtuales distintos.**
   - Windows: la guardia del monitor virtual **reclama** un destino del driver que no use otra sesión del proceso. Los cambios de topología (CCD) se serializan con el mismo cerrojo. Hace falta que el driver VDD tenga al menos N monitores (`monitors/count` en su XML; cambiarlo requiere administrador). El agente lo comprueba antes de conectar y, si no hay bastantes, da un error claro.
   - Linux (GNOME): cada sesión crea su propio monitor virtual de Mutter (`RecordVirtual`). No hace falta configurar nada.
5. **Solo en «extender».** En duplicar no tiene sentido varias veces la misma pantalla.
6. **Todo o nada al arrancar.** El agente prepara y conecta todos los monitores antes de arrancar ninguno. Si falla uno, suelta los anteriores. `StopStream` para todos. El grupo termina cuando terminan todos, con un informe por monitor.
7. **Interfaz:**
   - IPC 1.6: `StartStream.monitors` y `StartReceiver.screens` (0 o ausente = 1, compatible con paneles anteriores).
   - Anuncio mDNS: campo `sc` con las pantallas del receptor, solo si son más de 1. Los visores que lanza el agente no se anuncian (`--no-announce`): ya lo hace el agente por ellos.
   - CLI `crossscreen stream --monitors N`, máximo 4.
   - El protocolo de sesión no cambia.

## Consecuencias

- **A favor:**
  - poco código nuevo y ningún cambio de protocolo;
  - un fallo o corte en un monitor no tira los demás;
  - cada uno reconecta por su cuenta.
- **En contra:**
  - N conexiones QUIC y N procesos de visor, con algo más de memoria;
  - los monitores no comparten reloj de presentación, aunque cada uno va a su propio ritmo y no se nota en monitores distintos;
  - en Windows hay que configurar el número de monitores del driver una vez, con permisos de administrador.
- **Revisable:** si más adelante hiciera falta sincronizar monitores (por ejemplo, vídeo que cruza de uno a otro), se puede pasar a varios flujos en una conexión. Los mensajes nuevos se añadirían sin romper el protocolo.
