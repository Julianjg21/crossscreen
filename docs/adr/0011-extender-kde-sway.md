# ADR-0011: Extender el escritorio en KDE Plasma y Sway

- **Estado:** Aceptada (2026-11-09)
- **Contexto:** F4c (fuente Linux en extender). Amplía el ADR-0007, que solo cubría GNOME (Mutter).

## Contexto

En Linux, «extender» crea un monitor virtual con la API D-Bus de Mutter (`RecordVirtual`), que solo existe en GNOME. Plasma y Sway tienen sus propios mecanismos:

- **KDE Plasma (5.27+):** el XDG Desktop Portal de KDE ofrece la fuente `VIRTUAL` en `ScreenCast`, que crea un monitor nuevo y lo comparte por PipeWire. Muestra el diálogo de compartir pantalla. El tamaño está fijado a 1920×1080 en el portal de KDE ([bug 485850](https://bugs.kde.org/show_bug.cgi?id=485850), [bug 512620](https://bugs.kde.org/show_bug.cgi?id=512620)).
- **Sway (wlroots):** `swaymsg create_output` crea una salida *headless* a la que se puede dar cualquier modo (`output … mode --custom WxH@Hz`). Su portal (`xdg-desktop-portal-wlr`) solo deja elegir la salida con su archivo de configuración o con un selector interactivo, así que no sirve para capturar una salida recién creada.

## Decisión

Al extender en Linux se elige el primer mecanismo disponible:

1. **GNOME:** Mutter, como hasta ahora (ADR-0007). Sin diálogo y con la resolución del receptor.
2. **Sway:** si hay `SWAYSOCK`, `swaymsg` y un compositor con `wlr-screencopy`:
   - **Creación:** se crea la salida *headless* al recibir `SinkCapabilities`, con la resolución y la frecuencia del receptor, y se coloca a la derecha de las salidas activas.
   - **Captura:** directamente con `wlr-screencopy-unstable-v1` (`wayland-client` en Rust puro, MIT): `copy_with_damage` (solo cuando cambia), memoria compartida (`memfd`), conversión a I420 BT.709 y el mismo codificador que el resto de Linux.
   - **Retirada:** con `output … unplug` al terminar. También si falla algo a mitad, gracias al `Drop`.
   - Si el usuario la retira, la sesión termina.
3. **KDE Plasma** (y cualquier escritorio cuyo portal anuncie la fuente `VIRTUAL`): portal con `SourceType::Virtual`, en el mismo paso previo a conectar que «duplicar», con el diálogo del sistema. La entrada remota va por el mismo portal (`RemoteDesktop`) si se pidió. El monitor mide lo que decida el escritorio (hoy 1920×1080) y el receptor escala.
4. **Ninguno:** error claro que dice en qué escritorios funciona extender y propone «duplicar».

## Consecuencias

- **A favor:**
  - extender funciona en los tres escritorios Wayland más usados sin componentes nuevos del sistema;
  - en Sway no hay diálogo y la resolución es la del receptor.
- **En contra:**
  - **Sway:** no hay entrada remota todavía (habría que usar `wlr-virtual-pointer` y `virtual-keyboard` con un mapa de teclado). Si se pide, se avisa y solo se transmite la imagen. La captura copia por CPU, sin DMA-BUF.
  - **KDE:** resolución fija de 1920×1080 y un diálogo en cada sesión. Mientras KDE no deje elegir el tamaño, no se puede igualar al receptor.
- **Pruebas:**
  - **Sway:** probado con Sway 1.9 *headless* en WSL. La salida se creó a la resolución del receptor y a la derecha de la existente. Un cambio de color se vio exacto en un visor Windows, sin pérdidas, y la salida se retiró al terminar, también tras un error.
  - **KDE:** sin probar en un Plasma real (necesita el diálogo). Queda pendiente para el dueño.
- **Revisable:**
  - entrada remota en Sway;
  - Hyprland (`hyprctl output create headless` y la misma captura).
