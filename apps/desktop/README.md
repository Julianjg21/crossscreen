# CrossScreen — panel de control (Tauri 2)

Interfaz gráfica de CrossScreen. Es una **capa fina**: no contiene lógica de negocio. Cada acción se
reenvía al agente (`crossscreen-agent`) por IPC local (`crates/cs-ipc`), y los eventos del agente
(PIN de emparejamiento, equipos descubiertos, cambios de pares) se muestran en vivo.

## Qué muestra

Solo funciones que ya funcionan de verdad; no hay datos simulados.

| Sección | Qué hace |
|---|---|
| **Este equipo** | Nombre, device ID, huella (corta y completa), versión del agente, directorio de datos y registro |
| **Equipos emparejados** | Lista del almacén de confianza; «Olvidar» con confirmación |
| **Emparejar** | «Mostrar PIN» (abre un puerto 2 min y espera) o «Introducir PIN» (IP:puerto + PIN) con resultado y huella |
| **Red local** | Equipos CrossScreen descubiertos por mDNS, en vivo; acceso directo a emparejar si muestran PIN |
| **Monitor virtual** | Lista de pantallas y mostrar/retirar el monitor virtual a una resolución elegida. Solo Windows (requiere Virtual-Display-Driver instalado con `crossscreen-helper`); en otros sistemas aparece deshabilitado con la explicación |
| **Transmitir pantalla** | Iniciar o detener la transmisión hacia un equipo emparejado, elegir extender/duplicar, resolución, frecuencia, bitrate y perfil de red; mostrar estadísticas reales de la sesión |
| **Recibir pantalla** | Escuchar en un puerto UDP, elegir pantalla completa y activar opcionalmente el portapapeles |

Si el agente no está en marcha, la UI muestra «Agente no iniciado» con un botón que lanza
`crossscreen-agent` si lo encuentra junto al ejecutable de la UI (o en `../debug`, `../release`).

La transmisión y la recepción desde el panel están disponibles en Windows. El equipo remoto debe
estar emparejado y, para recibir, debe ejecutar también el visor compatible con su plataforma.
El portapapeles permanece desactivado por defecto y solo se sincroniza al activar la casilla en
ambos extremos.

## Requisitos

- Rust estable y, en Windows, las MSVC Build Tools y WebView2 (incluido en Windows 11).
- Node.js 20+ y npm.

## Ejecutar en desarrollo

```sh
# 1. Compilar y lanzar el agente (en otra terminal; se queda en primer plano)
cargo run -p crossscreen-agent

# 2. Dependencias del frontend (una vez)
cd apps/desktop
npm install

# 3. UI con recarga en caliente (Vite + Tauri)
npm run tauri dev
```

Diagnóstico sin UI: `cargo run -p crossscreen-agent -- --probe` se conecta al agente y muestra
estado, identidad, pares y pantallas.

## Compilar un ejecutable

```sh
cd apps/desktop
npm run build                                   # frontend → dist/
npx tauri build --debug --no-bundle             # o sin --debug para release
cargo build -p crossscreen-agent                # el agente debe quedar junto a la UI
```

El ejecutable queda en `<target>/debug/crossscreen-desktop(.exe)`, junto a `crossscreen-agent`.
Ojo: `cargo build -p crossscreen-desktop` a secas compila en modo desarrollo y la ventana intentará
cargar `http://localhost:1420` (el servidor de Vite); para embeber `dist/` hace falta la feature
`custom-protocol`, que `tauri build` activa automáticamente.

## Instalador Linux

El workflow de release genera un `.AppImage` y un paquete `.deb` para Linux
x86_64. El panel incluye el agente y el visor Linux como recursos y puede iniciarlos desde
la propia interfaz. La captura solicita autorización mediante el portal de
escritorio GNOME; el modo `extend` todavía no está disponible en Linux.

Para generar los instaladores localmente desde Ubuntu:

```sh
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libpipewire-0.3-dev pkg-config
./scripts/package-linux-desktop.sh
```

El instalador no redistribuye OpenH264. Después de instalar, ejecuta el
`fetch-openh264.sh` incluido para descargar el binario oficial de Cisco.

## Seguridad de la IPC

- Windows: tubería `\\.\pipe\crossscreen-agent-<SID del usuario>` con DACL que solo concede acceso
  al usuario actual y a SYSTEM, `PIPE_REJECT_REMOTE_CLIENTS`, instancia única
  (`FILE_FLAG_FIRST_PIPE_INSTANCE`) y verificación del propietario de la tubería en el cliente.
- Linux: socket Unix en `$XDG_RUNTIME_DIR/crossscreen/` (directorio `0700`) con comprobación del
  UID del par (`SO_PEERCRED`). **Implementado pero aún sin probar en Linux.**
