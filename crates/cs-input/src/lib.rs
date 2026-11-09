//! Captura e inyección de entrada remota (F6: ratón y teclado).
//!
//! Este crate traduce eventos del protocolo (`cs-protocol`) a inyección del SO
//! en el emisor (`SendInput` en Windows) acotada al rectángulo del monitor
//! transmitido.
//!
//! Piezas portables (sin I/O, testeables en cualquier SO):
//! - [`ScreenRect::map_normalized`]: `0..=65535` → píxel dentro del monitor.
//! - [`VirtualDesktop::to_absolute`]: píxel → coordenada absoluta `0..=65535`
//!   del escritorio virtual de Windows (`MOUSEEVENTF_VIRTUALDESK`).
//! - [`PressedState`]: qué teclas y botones siguen pulsados, para liberarlos
//!   todos si la sesión se corta y que nada quede "pegado" en el emisor.
//! - [`validate_event`]: rechaza eventos que no se pueden inyectar con sentido.

use cs_protocol::control::StreamId;
pub use cs_protocol::control::{ButtonState, InputEvent, MouseButton};
use std::collections::BTreeMap;

pub mod evdev;
#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{
    current_virtual_desktop, is_per_monitor_dpi_aware, make_process_per_monitor_dpi_aware,
    SendInputInjector,
};

/// Máximo de la escala normalizada (protocolo y API absoluta de Windows).
pub const NORM_MAX: u16 = u16::MAX;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputError {
    #[error("rectángulo de pantalla inválido: {0:?}")]
    InvalidRect(ScreenRect),
    #[error("evento de entrada inválido: {0}")]
    InvalidEvent(&'static str),
    #[error("{call} falló con código {code}")]
    Os { call: &'static str, code: i64 },
    /// `SendInput` insertó menos eventos de los pedidos. Suele ser UIPI: la
    /// ventana en primer plano pertenece a un proceso con más integridad
    /// (p. ej. elevado) y Windows bloquea la entrada sintética.
    #[error("el SO solo aceptó {sent} de {expected} eventos (¿ventana elevada en primer plano?)")]
    Blocked { sent: u32, expected: u32 },
    #[error("inyección de entrada no soportada en esta plataforma todavía")]
    Unsupported,
}

/// Rectángulo de un monitor en coordenadas del escritorio virtual (píxeles
/// físicos). En Windows puede tener origen negativo (monitores a la izquierda
/// o encima del principal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ScreenRect {
    pub fn validate(&self) -> Result<(), InputError> {
        let fits = i64::from(self.x) + i64::from(self.width) <= i64::from(i32::MAX)
            && i64::from(self.y) + i64::from(self.height) <= i64::from(i32::MAX);
        if self.width <= 0 || self.height <= 0 || !fits {
            return Err(InputError::InvalidRect(*self));
        }
        Ok(())
    }

    /// Convierte coordenadas normalizadas `0..=65535` a un píxel del monitor.
    /// `0` es el primer píxel y `65535` el último (`x + width - 1`), con
    /// redondeo al más cercano. Precondición: rectángulo válido.
    pub fn map_normalized(&self, x_norm: u16, y_norm: u16) -> (i32, i32) {
        (
            self.x + scale(x_norm, self.width),
            self.y + scale(y_norm, self.height),
        )
    }
}

/// `n` en `0..=65535` → `0..=len-1`, redondeando al más cercano.
fn scale(n: u16, len: i32) -> i32 {
    let span = i64::from(len.max(1) - 1);
    let max = i64::from(NORM_MAX);
    ((i64::from(n) * span + max / 2) / max) as i32
}

/// Escritorio virtual de Windows (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`,
/// `SM_CXVIRTUALSCREEN`, `SM_CYVIRTUALSCREEN`): la unión de todos los monitores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl VirtualDesktop {
    /// Píxel del escritorio → coordenada absoluta `0..=65535` para
    /// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
    ///
    /// Windows convierte de vuelta con `píxel = origen + (abs × ancho) / 65536`
    /// (truncando). Se usa el **techo** de `(p − origen) × 65536 / ancho` para
    /// que esa conversión inversa caiga exactamente en `p` (lo verifican los
    /// tests para cada píxel). Los valores fuera del escritorio se recortan.
    pub fn to_absolute(&self, px: i32, py: i32) -> (u16, u16) {
        (
            to_abs_axis(px, self.x, self.width),
            to_abs_axis(py, self.y, self.height),
        )
    }

    /// Conversión inversa que aplica Windows (para tests y diagnóstico).
    pub fn from_absolute(&self, ax: u16, ay: u16) -> (i32, i32) {
        (
            self.x + ((i64::from(ax) * i64::from(self.width)) >> 16) as i32,
            self.y + ((i64::from(ay) * i64::from(self.height)) >> 16) as i32,
        )
    }
}

fn to_abs_axis(p: i32, origin: i32, len: i32) -> u16 {
    let len = i64::from(len.max(1));
    let rel = (i64::from(p) - i64::from(origin)).clamp(0, len - 1);
    let abs = (rel * 65536 + len - 1) / len;
    abs.clamp(0, i64::from(NORM_MAX)) as u16
}

/// Valida que un evento se puede inyectar con sentido.
pub fn validate_event(event: &InputEvent) -> Result<(), InputError> {
    match event {
        InputEvent::Key { scancode, .. } => {
            // Conjunto 1 de un byte (+ prefijo E0 vía `extended`). Las
            // secuencias E1 (Pausa) y los valores vacíos no son inyectables así.
            if *scancode == 0 || *scancode > 0xFF {
                return Err(InputError::InvalidEvent("scan code fuera de 0x01..=0xFF"));
            }
        }
        InputEvent::MouseWheel {
            delta_x: 0,
            delta_y: 0,
            ..
        } => {
            return Err(InputError::InvalidEvent("rueda sin desplazamiento"));
        }
        InputEvent::MouseMove { .. }
        | InputEvent::MouseButton { .. }
        | InputEvent::MouseWheel { .. } => {}
    }
    Ok(())
}

/// Inyector de entrada en el SO del emisor.
pub trait InputInjector {
    /// Inyecta `event`; las coordenadas del ratón se acotan a `target_rect`
    /// (el monitor transmitido en el escritorio virtual).
    fn inject(&mut self, event: &InputEvent, target_rect: ScreenRect) -> Result<(), InputError>;

    /// Libera todo lo que siga pulsado (fin de sesión, red perdida).
    fn release_all(&mut self) -> Result<(), InputError>;
}

/// Tecla identificada por su *scan code* y el prefijo E0.
type KeyId = (u16, bool);

/// Registro de teclas y botones pulsados.
///
/// Se alimenta con cada evento inyectado; [`PressedState::release_all`]
/// devuelve los eventos de liberación necesarios para dejar el emisor limpio
/// (primero teclas, para que un modificador no altere el clic, luego botones),
/// en orden determinista.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PressedState {
    keys: BTreeMap<KeyId, StreamId>,
    buttons: BTreeMap<MouseButton, StreamId>,
}

impl PressedState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Actualiza el registro con un evento ya inyectado. Las repeticiones de
    /// pulsación (autorrepetición) no duplican entradas; soltar algo no pulsado
    /// no tiene efecto.
    pub fn record(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key {
                stream_id,
                scancode,
                extended,
                state,
            } => match state {
                ButtonState::Pressed => {
                    self.keys.insert((scancode, extended), stream_id);
                }
                ButtonState::Released => {
                    self.keys.remove(&(scancode, extended));
                }
            },
            InputEvent::MouseButton {
                stream_id,
                button,
                state,
            } => match state {
                ButtonState::Pressed => {
                    self.buttons.insert(button, stream_id);
                }
                ButtonState::Released => {
                    self.buttons.remove(&button);
                }
            },
            InputEvent::MouseMove { .. } | InputEvent::MouseWheel { .. } => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    pub fn pressed_keys(&self) -> impl Iterator<Item = (u16, bool)> + '_ {
        self.keys.keys().copied()
    }

    pub fn pressed_buttons(&self) -> impl Iterator<Item = MouseButton> + '_ {
        self.buttons.keys().copied()
    }

    /// Devuelve los eventos de liberación de todo lo pulsado y vacía el registro.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(self.keys.len() + self.buttons.len());
        for ((scancode, extended), stream_id) in std::mem::take(&mut self.keys) {
            out.push(InputEvent::Key {
                stream_id,
                scancode,
                extended,
                state: ButtonState::Released,
            });
        }
        for (button, stream_id) in std::mem::take(&mut self.buttons) {
            out.push(InputEvent::MouseButton {
                stream_id,
                button,
                state: ButtonState::Released,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: ScreenRect = ScreenRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn map_normalized_edges() {
        assert_eq!(FULL_HD.map_normalized(0, 0), (0, 0));
        assert_eq!(FULL_HD.map_normalized(65535, 65535), (1919, 1079));
        // Centro: 32768/65535 × 1919 ≈ 959,5 → 960 (redondeo al más cercano).
        assert_eq!(FULL_HD.map_normalized(32768, 32768), (960, 540));
    }

    #[test]
    fn map_normalized_negative_origin_and_multimonitor() {
        // Monitor a la izquierda del principal (origen negativo).
        let left = ScreenRect {
            x: -1366,
            y: 0,
            width: 1366,
            height: 768,
        };
        assert_eq!(left.map_normalized(0, 0), (-1366, 0));
        assert_eq!(left.map_normalized(65535, 65535), (-1, 767));
        // Monitor encima y desplazado.
        let above = ScreenRect {
            x: 200,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        assert_eq!(above.map_normalized(0, 65535), (200, -1));
        // Monitor virtual a la derecha del principal (caso típico de CrossScreen).
        let right = ScreenRect {
            x: 1920,
            y: 0,
            width: 1600,
            height: 1000,
        };
        assert_eq!(right.map_normalized(65535, 0), (3519, 0));
    }

    #[test]
    fn map_normalized_is_monotonic_and_in_bounds() {
        let r = ScreenRect {
            x: -37,
            y: 11,
            width: 1366,
            height: 7,
        };
        let mut last = i32::MIN;
        for n in (0..=65535u32).step_by(7).chain([65535]) {
            let (x, y) = r.map_normalized(n as u16, n as u16);
            assert!(x >= r.x && x < r.x + r.width);
            assert!(y >= r.y && y < r.y + r.height);
            assert!(x >= last);
            last = x;
        }
    }

    #[test]
    fn one_pixel_rect() {
        let r = ScreenRect {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        };
        assert_eq!(r.map_normalized(0, 0), (5, 5));
        assert_eq!(r.map_normalized(65535, 65535), (5, 5));
    }

    #[test]
    fn rect_validation() {
        assert!(FULL_HD.validate().is_ok());
        for bad in [
            ScreenRect {
                width: 0,
                ..FULL_HD
            },
            ScreenRect {
                height: -1,
                ..FULL_HD
            },
            ScreenRect {
                x: i32::MAX - 10,
                ..FULL_HD
            },
        ] {
            assert_eq!(bad.validate(), Err(InputError::InvalidRect(bad)));
        }
    }

    /// Escritorio virtual de dos monitores: uno de 1366×768 a la izquierda del
    /// principal de 1920×1080.
    const TWO_MON: VirtualDesktop = VirtualDesktop {
        x: -1366,
        y: 0,
        width: 1366 + 1920,
        height: 1080,
    };

    #[test]
    fn to_absolute_roundtrips_every_pixel_through_windows_formula() {
        for vd in [
            TWO_MON,
            VirtualDesktop {
                x: 0,
                y: -1440,
                width: 7680,
                height: 2520,
            },
            VirtualDesktop {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        ] {
            for px in vd.x..vd.x + vd.width {
                let (ax, _) = vd.to_absolute(px, vd.y);
                assert_eq!(vd.from_absolute(ax, 0).0, px, "x={px} en {vd:?}");
            }
            for py in vd.y..vd.y + vd.height {
                let (_, ay) = vd.to_absolute(vd.x, py);
                assert_eq!(vd.from_absolute(0, ay).1, py, "y={py} en {vd:?}");
            }
        }
    }

    #[test]
    fn to_absolute_edges_and_clamping() {
        assert_eq!(TWO_MON.to_absolute(-1366, 0), (0, 0));
        // Último píxel del escritorio: el resultado (u16) cabe en 0..=65535 por
        // tipo; lo que importa es que Windows lo convierta de vuelta exacto.
        let (ax, ay) = TWO_MON.to_absolute(1919, 1079);
        assert_eq!(TWO_MON.from_absolute(ax, ay), (1919, 1079));
        // Fuera del escritorio: se recorta al borde.
        assert_eq!(TWO_MON.to_absolute(-5000, -5), (0, 0));
        assert_eq!(
            TWO_MON
                .from_absolute(TWO_MON.to_absolute(99_999, 99_999).0, 0)
                .0,
            1919
        );
    }

    #[test]
    fn normalized_to_absolute_lands_on_target_monitor() {
        // Monitor virtual de CrossScreen a la derecha del principal.
        let vd = VirtualDesktop {
            x: 0,
            y: 0,
            width: 1920 + 1600,
            height: 1080,
        };
        let target = ScreenRect {
            x: 1920,
            y: 0,
            width: 1600,
            height: 1000,
        };
        for (nx, ny) in [(0, 0), (65535, 65535), (12345, 54321)] {
            let (px, py) = target.map_normalized(nx, ny);
            let (ax, ay) = vd.to_absolute(px, py);
            assert_eq!(vd.from_absolute(ax, ay), (px, py));
        }
    }

    fn key(sc: u16, ext: bool, state: ButtonState) -> InputEvent {
        InputEvent::Key {
            stream_id: 1,
            scancode: sc,
            extended: ext,
            state,
        }
    }

    fn btn(b: MouseButton, state: ButtonState) -> InputEvent {
        InputEvent::MouseButton {
            stream_id: 1,
            button: b,
            state,
        }
    }

    #[test]
    fn release_all_releases_keys_then_buttons_in_order() {
        use ButtonState::*;
        let mut s = PressedState::new();
        for ev in [
            btn(MouseButton::Right, Pressed),
            key(0x1D, false, Pressed), // Ctrl izquierdo
            key(0x1D, true, Pressed),  // Ctrl derecho (E0)
            key(0x2A, false, Pressed), // Mayús izquierda
            btn(MouseButton::Left, Pressed),
            key(0x2A, false, Released),
        ] {
            s.record(&ev);
        }
        assert_eq!(
            s.release_all(),
            vec![
                key(0x1D, false, Released),
                key(0x1D, true, Released),
                btn(MouseButton::Left, Released),
                btn(MouseButton::Right, Released),
            ]
        );
        assert!(s.is_empty());
        assert!(s.release_all().is_empty(), "segunda llamada sin efecto");
    }

    #[test]
    fn autorepeat_and_spurious_release_are_harmless() {
        use ButtonState::*;
        let mut s = PressedState::new();
        for _ in 0..5 {
            s.record(&key(0x1E, false, Pressed)); // 'A' con autorrepetición
        }
        s.record(&btn(MouseButton::Middle, Released)); // nunca pulsado
        s.record(&InputEvent::MouseMove {
            stream_id: 1,
            x_norm: 1,
            y_norm: 2,
        });
        assert_eq!(s.pressed_keys().collect::<Vec<_>>(), vec![(0x1E, false)]);
        assert_eq!(s.pressed_buttons().count(), 0);
        assert_eq!(s.release_all(), vec![key(0x1E, false, Released)]);
    }

    #[test]
    fn release_keeps_stream_id_of_press() {
        let mut s = PressedState::new();
        s.record(&InputEvent::Key {
            stream_id: 7,
            scancode: 0x38,
            extended: false,
            state: ButtonState::Pressed,
        });
        assert_eq!(s.release_all()[0].stream_id(), 7);
    }

    #[test]
    fn event_validation() {
        use ButtonState::*;
        assert!(validate_event(&key(0x1E, false, Pressed)).is_ok());
        assert!(validate_event(&key(0xFF, true, Released)).is_ok());
        assert!(validate_event(&key(0, false, Pressed)).is_err());
        assert!(validate_event(&key(0x100, false, Pressed)).is_err());
        let wheel = |dx, dy| InputEvent::MouseWheel {
            stream_id: 1,
            delta_x: dx,
            delta_y: dy,
        };
        assert!(validate_event(&wheel(0, 0)).is_err());
        assert!(validate_event(&wheel(0, -120)).is_ok());
        assert!(validate_event(&wheel(120, 0)).is_ok());
        assert!(validate_event(&btn(MouseButton::Back, Pressed)).is_ok());
    }
}
