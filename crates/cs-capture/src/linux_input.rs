//! Acción de entrada ya traducida a Linux (códigos evdev y píxeles del
//! stream), común al portal `RemoteDesktop` y a la API de Mutter.

/// Acción de entrada para inyectar en la sesión de GNOME.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputAction {
    /// Puntero a la posición `(x, y)` en píxeles del stream capturado.
    PointerAbs { x: f64, y: f64 },
    /// Botón del ratón (`BTN_*` de evdev).
    Button { code: u32, pressed: bool },
    /// Rueda en escalones discretos (`y` positivo = abajo, `x` = derecha).
    Axis { horizontal: bool, steps: i32 },
    /// Tecla (`KEY_*` de evdev).
    Key { code: u32, pressed: bool },
}
