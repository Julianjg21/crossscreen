# ADR-0007: Monitor virtual en Linux: backends por compositor con EVDI de respaldo

**Estado:** Propuesta (validar con la PoC-7) · **Fecha:** 2026-11-08

## Contexto
En Wayland, KMS lo controla el compositor y no existe una API estándar para crear monitores.

En las referencias:
- **TwinDesk** solo soporta GNOME (Mutter `RecordVirtual`) y, en X11, recurre a forzar conectores físicos libres como root sin contraseña.
- **Weylus** solo documenta una configuración manual de xrandr.
- **ScreenExtend** no lo implementa.

Detalle en [03-monitores-virtuales.md](../03-monitores-virtuales.md).

## Decisión
Implementar `VirtualDisplayProvider` con estos backends, elegidos automáticamente por orden de prioridad:

1. `mutter-virtual`: D-Bus ScreenCast `RecordVirtual` (GNOME).
2. `kwin-virtual`: `zkde_screencast_unstable_v1.stream_virtual_output` o el portal con `VIRTUAL` (KDE Plasma 6).
3. `wlroots-headless`: Sway (`create_output`) y Hyprland (`output create headless`).
4. `evdi`: respaldo universal, incluido X11. Usa el módulo de la distribución y `libevdi` enlazada dinámicamente.

Si ningún backend es viable, se muestra un diagnóstico con los pasos a seguir. **Nunca se cae en silencio al modo duplicar.**

## Alternativas
- **Solo EVDI.** Universal, pero exige DKMS y firma para Secure Boot, y copia por CPU. Es una mala experiencia por defecto en GNOME y KDE, donde existe una vía sin privilegios.
- **Forzar conectores con sysfs o xorg.conf.** Frágil y requiere escribir en el sistema como root. Se descarta.

## Consecuencias
- Cuatro backends que mantener, con APIs semiprivadas (Mutter, KWin). Hace falta una matriz de versiones en CI o en el laboratorio.
- Es la parte de mayor riesgo del proyecto: R1 en [06-riesgos.md](../06-riesgos.md).
