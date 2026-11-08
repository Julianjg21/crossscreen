# PoC-1: Monitores virtuales en Windows con Virtual-Display-Driver (VDD)

**Estado:** completada en lo esencial, con un cambio de diseño.
**Pendiente:** la prueba de suspensión/reanudación y los 100 ciclos completos. Se hicieron 10 ciclos de topología y 3 recargas.
**Fecha:** 2026-11-08.

**Entorno:**
- Windows 11 Pro 10.0.26200, portátil ASUS con una sola GPU (Intel Iris Xe) y panel 1920×1080 al 125 %.
- VDD 25.7.23 (DriverVer 11.30.4.434, IddCx 1.2), verificado por SHA-256 y Authenticode (SignPath Foundation).

**Instalación:** `crossscreen-helper vdd-install` (Rust, SetupAPI). Sin `devcon` ni `nefconc`, sin certificados raíz y sin tareas programadas.

## Resultados

| Pregunta | Resultado | Evidencia |
|---|---|---|
| ¿Instalación sin reiniciar? | ✅ 16 s incluido el UAC; `reboot=false` | `helper-install.log` |
| ¿El SO lo reconoce como monitor real? | ✅ Aparece el adaptador "Virtual Display Driver" y el monitor `MTT1337` (EDID "VDD by MTT") | PnP + CCD |
| ¿Extiende el escritorio al instalarse? | ❌ **Windows lo pone en duplicar** (clon de `DISPLAY1`), también tras cada cambio de cantidad | CCD: dos rutas con la misma fuente |
| ¿Se puede forzar extender? | ✅ `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` sin privilegios (rc 0) | — |
| ¿Funciona `DisplaySwitch.exe /extend`? | ❌ No cambió nada y no informa del error | — |
| ¿Modo inicial correcto? | ❌ Windows elige 800×600 a 30 Hz; hay que fijarlo con `ChangeDisplaySettingsEx` (sin privilegios, ~60 ms) | — |
| ¿Resoluciones arbitrarias? | ✅ Añadiendo `<resolution>` al XML y reiniciando el dispositivo. Se probó 1600×1000, combinable con todas las frecuencias globales | `EnumDisplaySettings` |
| ¿Varios monitores? | ✅ `count=2` crea `UID256` y `UID257`; se extendieron los dos | CCD |
| ¿Retirar o mostrar sin desinstalar? | ✅ `SDC_TOPOLOGY_INTERNAL` / `EXTEND`: **10/10 ciclos sin fallos**, ~0,25–0,3 s cada uno (incluido el sondeo) | Tabla de ciclos |
| ¿Recuerda Windows el modo por configuración? | ✅ Al volver a un monitor recuperó 1600×1000 | — |
| ¿Cambiar la configuración en caliente sin admin (tubería)? | ❌ **No es viable**, ver abajo | WER |
| ¿Reinicio limpio del dispositivo? | ✅ `pnputil /restart-device` (admin): 4,2 s con UAC, sin caídas | WER antes y después |

## Hallazgo crítico: los comandos de la tubería tumban el driver

`RELOAD_DRIVER` y `SETDISPLAYCOUNT` **sí aplican** la configuración nueva. Pero lo consiguen porque el host UMDF se cae (`WUDFUnhandledException`) y Windows lo reinicia, que es lo que tarda unos 5 s:

- Se registraron **6 caídas, exactamente una por comando**.
- Tras 3 caídas en ~15 s, Windows **deshabilitó el dispositivo (código 43)**.
- Causa probable, vista en el código upstream: `ReloadDriver(hPipe)` llama a `WdfObjectGet_IndirectDeviceContextWrapper` con el **handle de la tubería**, que no es un objeto WDF.
- Además, `PING` no responde en esta versión.

**Consecuencia:** CrossScreen **no usará la tubería de VDD**.

## Hallazgo de seguridad

La tubería `\\.\pipe\MTTVirtualDisplayPipe` tiene la DACL `D:(A;;GA;;;WD)` (Everyone, control total) y no usa `PIPE_REJECT_REMOTE_CLIENTS`. Se verificó que un proceso **sin elevar** puede conectarse.

- **Riesgo:** cualquier proceso local, o un usuario autenticado por SMB, puede reconfigurar o **tumbar** el driver. Un DoS local trivial: tres comandos lo dejan en código 43.
- **Mitigación disponible hoy:** ninguna sin modificar el driver. Se documenta y refuerza el plan del driver propio (F8).

## Diseño resultante (actualiza el ADR-0006)

| Operación | Mecanismo | Privilegios | Frecuencia |
|---|---|---|---|
| Instalar el driver | `crossscreen-helper vdd-install` | Admin (UAC) | Una vez |
| Definir modos y cantidad de monitores | El helper escribe el XML con un parser real y reinicia el dispositivo (`vdd-configure`, pendiente) | Admin (UAC) | Rara: al instalar se preaprovisionan resoluciones comunes y N monitores, y al emparejar un receptor con resolución nueva se añade la suya |
| Mostrar el monitor (extender) | CCD `SetDisplayConfig` con **rutas explícitas**: activar solo el destino virtual elegido. `SDC_TOPOLOGY_EXTEND` activa todos | Usuario | En cada sesión |
| Fijar el modo | `ChangeDisplaySettingsEx` sobre el GDI name del destino | Usuario | En cada sesión |
| Retirar el monitor al cerrar la sesión | CCD: desactivar la ruta del destino virtual | Usuario | En cada sesión |
| Recuperar un dispositivo en código 43 | `crossscreen-helper vdd-restart` | Admin (UAC) | Excepcional; se detecta por el estado PnP |

Así **cada sesión normal no requiere elevación**. Solo instalar y reconfigurar los modos piden UAC.

## Implementación de producción: `cs-display` (Rust, CCD con rutas explícitas)

`crossscreen display show|hide|list|cycle`. El código está en `crates/cs-display/src/windows_ccd.rs`.

**Funcionamiento:**
- Activa **solo** la ruta del destino virtual, con una fuente propia libre del mismo adaptador.
- Si Windows dejó el monitor clonando otra fuente, elimina esa ruta clonada.
- Fija el modo con `ChangeDisplaySettingsExW` y verifica antes que el modo existe.

**Resultado: 100 ciclos de mostrar/retirar sin privilegios, 0 fallos, 0 caídas del driver.**

| Operación | p50 | p95 | máx |
|---|---|---|---|
| Mostrar (topología + modo 1920×1080@60) | 299 ms | 536 ms | 763 ms |
| Retirar | 76 ms | 105 ms | 314 ms |

## Pendiente
1. Prueba de suspensión e hibernación con el monitor extendido.
2. ~~100 ciclos con CCD de rutas explícitas~~ ✅ hecho.
3. Probar en un equipo con GPU dedicada y en uno híbrido (riesgo R8).
4. Medir cuánto tarda el reinicio del dispositivo sin contar el UAC.
5. Probar la desinstalación (`vdd-uninstall --purge-config`) y la reinstalación.
