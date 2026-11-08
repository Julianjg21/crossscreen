# ADR-0006: Monitor virtual en Windows: VDD externo ahora, IddCx propio después

**Estado:** Propuesta (validar con la PoC-1) · **Fecha:** 2026-11-08

## Contexto
Un monitor virtual real en Windows requiere un driver IddCx firmado. Escribir y firmar uno propio exige un certificado EV, la atestación de Microsoft y mantenimiento continuo.

En las referencias:
- **TwinDesk** usa VirtualDrivers/Virtual-Display-Driver (MIT, firmado por SignPath) con éxito.
- **ScreenExtend** usa un fork no publicado de virtual-display-rs e instala un certificado raíz autofirmado, una práctica inaceptable.

Detalle en [03-monitores-virtuales.md](../03-monitores-virtuales.md).

## Decisión
- **Fases 1–3:**
  - instalar **Virtual-Display-Driver** upstream en tiempo de ejecución, fijado por versión y SHA-256, comprobando el sujeto de la firma Authenticode;
  - la instalación la hace un helper privilegiado bajo demanda;
  - controlarlo desde el agente (configuración XML con un *parser* real y topología CCD).
- **Fase 8:** driver IddCx propio (IddCx 1.5+), si el proyecto lo justifica. Objetivos:
  - IPC propia;
  - modos dinámicos;
  - acceso directo a la *swap chain*;
  - selección del adaptador;
  - firma de Microsoft (necesaria para ARM64).

## Revisión tras la PoC-1 (2026-11-08)
Ver [poc-01-vdd.md](../poc-results/poc-01-vdd.md). Se mantiene VDD para las fases 1–3, con estos cambios:
- **No se usa la tubería de control de VDD.** `RELOAD_DRIVER` y `SETDISPLAYCOUNT` aplican los cambios tumbando el host UMDF. Tras 3 caídas, Windows deshabilita el dispositivo.
- Los cambios de modos y de cantidad se hacen con el helper (XML + `pnputil /restart-device`), con UAC y de forma poco frecuente (preaprovisionamiento).
- Mostrar y retirar el monitor en cada sesión se hace con CCD (`SetDisplayConfig` con rutas explícitas) y `ChangeDisplaySettingsEx`, **sin privilegios**.
- Windows pone en **duplicar** cada monitor nuevo, así que la topología se aplica siempre de forma explícita.
- La DACL "Everyone" de la tubería de VDD es un riesgo aceptado temporalmente y adelanta la prioridad del driver propio (F8).

## Prohibido
- Instalar certificados raíz en el almacén del sistema.
- Registrar tareas como SYSTEM que el usuario pueda lanzar.
- Editar XML con regex.

## Consecuencias
- Dependemos de un proyecto de terceros: hay que monitorizar sus releases y probar cada actualización antes de fijar una versión nueva.
- Windows on ARM queda fuera del alcance hasta la fase 8.
