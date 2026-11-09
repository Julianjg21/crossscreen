//! Traducción de eventos del protocolo a códigos de Linux (evdev), para
//! inyectar entrada en GNOME (portal `RemoteDesktop` o API de Mutter).
//!
//! Los *scan codes* del conjunto 1 (los que envía el visor) coinciden con
//! los códigos evdev para las teclas sin prefijo (Esc=1, A=30, F12=88…);
//! las extendidas (prefijo E0) necesitan una tabla.

use cs_protocol::control::MouseButton;

/// Unidades de rueda de Windows por escalón.
pub const WHEEL_STEP: i32 = 120;

/// Código evdev (`KEY_*`) de un *scan code* del conjunto 1. `None` si no
/// tiene equivalente.
pub fn keycode(scancode: u16, extended: bool) -> Option<u32> {
    if scancode == 0 || scancode > 0x7F {
        return None;
    }
    if !extended {
        // 0x01..=0x58 coinciden; 0x59.. son teclas raras sin código fijo.
        return (scancode <= 0x58).then_some(u32::from(scancode));
    }
    let code = match scancode {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ (Impr Pant)
        0x38 => 100, // KEY_RIGHTALT (AltGr)
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA (Windows izquierda)
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE (menú contextual)
        0x20 => 113, // KEY_MUTE
        0x2E => 114, // KEY_VOLUMEDOWN
        0x30 => 115, // KEY_VOLUMEUP
        0x22 => 164, // KEY_PLAYPAUSE
        0x24 => 166, // KEY_STOPCD
        0x10 => 165, // KEY_PREVIOUSSONG
        0x19 => 163, // KEY_NEXTSONG
        _ => return None,
    };
    Some(code)
}

/// Código evdev (`BTN_*`) de un botón del ratón.
pub fn button(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,    // BTN_LEFT
        MouseButton::Right => 0x111,   // BTN_RIGHT
        MouseButton::Middle => 0x112,  // BTN_MIDDLE
        MouseButton::Back => 0x113,    // BTN_SIDE
        MouseButton::Forward => 0x114, // BTN_EXTRA
    }
}

/// Acumula la rueda en unidades de Windows (120 por escalón) y la convierte
/// en escalones discretos, conservando el resto para ruedas de alta
/// resolución y touchpads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WheelAccumulator {
    x: i32,
    y: i32,
}

impl WheelAccumulator {
    /// Devuelve `(escalones_x, escalones_y)` en el sentido de Linux:
    /// `y` positivo = hacia abajo (Windows: positivo = hacia arriba);
    /// `x` positivo = derecha en ambos.
    pub fn push(&mut self, delta_x: i16, delta_y: i16) -> (i32, i32) {
        self.x += i32::from(delta_x);
        self.y += i32::from(delta_y);
        let sx = self.x / WHEEL_STEP;
        let sy = self.y / WHEEL_STEP;
        self.x -= sx * WHEEL_STEP;
        self.y -= sy * WHEEL_STEP;
        (sx, -sy)
    }
}

/// Coordenada normalizada del protocolo (`0..=65535`) → píxel del stream
/// de `size` píxeles (`0..size`).
pub fn denormalize(norm: u16, size: u32) -> f64 {
    if size == 0 {
        return 0.0;
    }
    f64::from(norm) / f64::from(u16::MAX) * f64::from(size - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_scancodes_match_evdev() {
        assert_eq!(keycode(0x01, false), Some(1)); // Esc
        assert_eq!(keycode(0x1E, false), Some(30)); // A
        assert_eq!(keycode(0x1C, false), Some(28)); // Enter
        assert_eq!(keycode(0x2A, false), Some(42)); // Mayús izquierda
        assert_eq!(keycode(0x57, false), Some(87)); // F11
        assert_eq!(keycode(0x58, false), Some(88)); // F12
        assert_eq!(keycode(0x59, false), None);
        assert_eq!(keycode(0, false), None);
    }

    #[test]
    fn extended_scancodes_use_table() {
        assert_eq!(keycode(0x48, true), Some(103)); // Arriba
        assert_eq!(keycode(0x53, true), Some(111)); // Supr
        assert_eq!(keycode(0x38, true), Some(100)); // AltGr
        assert_eq!(keycode(0x1D, true), Some(97)); // Ctrl derecho
        assert_eq!(keycode(0x5B, true), Some(125)); // Windows
        assert_eq!(keycode(0x1E, true), None);
    }

    #[test]
    fn buttons() {
        assert_eq!(button(MouseButton::Left), 0x110);
        assert_eq!(button(MouseButton::Forward), 0x114);
    }

    #[test]
    fn wheel_steps_and_direction() {
        let mut w = WheelAccumulator::default();
        // Un escalón hacia arriba en Windows = -1 en Linux.
        assert_eq!(w.push(0, 120), (0, -1));
        assert_eq!(w.push(0, -240), (0, 2));
        // Alta resolución: 40 + 40 + 40 = un escalón.
        assert_eq!(w.push(0, 40), (0, 0));
        assert_eq!(w.push(0, 40), (0, 0));
        assert_eq!(w.push(0, 40), (0, -1));
        assert_eq!(w.push(120, 0), (1, 0));
    }

    #[test]
    fn denormalize_covers_the_stream() {
        assert_eq!(denormalize(0, 1920), 0.0);
        assert_eq!(denormalize(u16::MAX, 1920), 1919.0);
        assert!((denormalize(32768, 1000) - 499.5).abs() < 0.1);
        assert_eq!(denormalize(100, 0), 0.0);
    }
}
