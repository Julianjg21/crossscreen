//! Inyección de entrada en Windows con `SendInput`.
//!
//! - Ratón: movimiento **absoluto sobre el escritorio virtual**
//!   (`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`), así
//!   funciona en cualquier monitor, incluidos los de origen negativo y el
//!   monitor virtual de CrossScreen.
//! - Teclado: por *scan code* (`KEYEVENTF_SCANCODE`), independiente de la
//!   distribución de teclado del emisor.
//!
//! Limitaciones del SO (no son errores de este código):
//! - UIPI: no se puede inyectar en ventanas de procesos con más integridad
//!   (p. ej. una app elevada en primer plano); `SendInput` devuelve menos
//!   eventos y se informa como [`InputError::Blocked`].
//! - El escritorio seguro (UAC, Ctrl+Alt+Supr, pantalla de bloqueo) no acepta
//!   entrada sintética desde la sesión del usuario.
#![allow(unsafe_code)] // FFI Win32; cada bloque unsafe está acotado y comentado.

use crate::{validate_event, InputError, InputInjector, PressedState, ScreenRect, VirtualDesktop};
use cs_protocol::control::{ButtonState, InputEvent, MouseButton};
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Indica si el hilo actual trabaja en píxeles físicos (*per-monitor DPI aware*).
///
/// **Requisito para inyectar bien:** los rectángulos de monitor que llegan de
/// DXGI/CCD están en píxeles físicos. Un proceso sin *DPI awareness* recibe de
/// `GetSystemMetrics` el escritorio virtual en coordenadas lógicas (escaladas,
/// p. ej. al 125 %), y el clic caería desplazado.
pub fn is_per_monitor_dpi_aware() -> bool {
    use windows::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
        DPI_AWARENESS_PER_MONITOR_AWARE,
    };
    // SAFETY: funciones sin punteros de entrada; el contexto lo gestiona el SO.
    unsafe {
        GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())
            == DPI_AWARENESS_PER_MONITOR_AWARE
    }
}

/// Activa *per-monitor DPI awareness v2* para todo el proceso. Debe llamarse
/// al arrancar, antes de crear ventanas; si ya estaba fijado, el SO lo rechaza
/// y se devuelve error (el estado actual se puede consultar con
/// [`is_per_monitor_dpi_aware`]).
pub fn make_process_per_monitor_dpi_aware() -> Result<(), InputError> {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // SAFETY: constante del SO; sin punteros propios.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.map_err(
        |e| InputError::Os {
            call: "SetProcessDpiAwarenessContext",
            code: i64::from(e.code().0),
        },
    )
}

/// Valores de `mouseData` para los botones laterales (winuser.h).
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

/// Escritorio virtual actual según el SO (cambia al conectar o retirar monitores).
pub fn current_virtual_desktop() -> VirtualDesktop {
    // SAFETY: GetSystemMetrics no recibe punteros y no tiene efectos secundarios.
    unsafe {
        VirtualDesktop {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as _,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Traduce un evento a estructuras `INPUT` (puro: no llama al SO).
pub fn build_inputs(
    event: &InputEvent,
    target: ScreenRect,
    desktop: VirtualDesktop,
) -> Result<Vec<INPUT>, InputError> {
    validate_event(event)?;
    let inputs = match *event {
        InputEvent::MouseMove { x_norm, y_norm, .. } => {
            target.validate()?;
            let (px, py) = target.map_normalized(x_norm, y_norm);
            let (ax, ay) = desktop.to_absolute(px, py);
            vec![mouse(
                i32::from(ax),
                i32::from(ay),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]
        }
        InputEvent::MouseButton { button, state, .. } => {
            let down = state == ButtonState::Pressed;
            let (flags, data) = match (button, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            };
            vec![mouse(0, 0, data, flags)]
        }
        InputEvent::MouseWheel {
            delta_x, delta_y, ..
        } => {
            // `mouseData` es un DWORD con signo en la práctica (WHEEL_DELTA = 120).
            let mut v = Vec::with_capacity(2);
            if delta_y != 0 {
                v.push(mouse(0, 0, i32::from(delta_y) as u32, MOUSEEVENTF_WHEEL));
            }
            if delta_x != 0 {
                v.push(mouse(0, 0, i32::from(delta_x) as u32, MOUSEEVENTF_HWHEEL));
            }
            v
        }
        InputEvent::Key {
            scancode,
            extended,
            state,
            ..
        } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if state == ButtonState::Released {
                flags |= KEYEVENTF_KEYUP;
            }
            vec![keyboard(scancode, flags)]
        }
    };
    Ok(inputs)
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: slice válido de INPUT inicializados; cbsize es el tamaño real.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        return Ok(());
    }
    if sent == 0 {
        // SAFETY: sin argumentos; lee el último error del hilo.
        let code = unsafe { GetLastError() };
        if code.0 != 0 && code.0 != 5 {
            return Err(InputError::Os {
                call: "SendInput",
                code: i64::from(code.0),
            });
        }
    }
    Err(InputError::Blocked {
        sent,
        expected: inputs.len() as u32,
    })
}

/// Inyector real con `SendInput`. Registra lo pulsado y lo libera al soltarse
/// (`Drop`) para no dejar teclas ni botones pegados si la sesión termina mal.
#[derive(Debug, Default)]
pub struct SendInputInjector {
    pressed: PressedState,
}

impl SendInputInjector {
    pub fn new() -> Self {
        if !is_per_monitor_dpi_aware() {
            tracing::warn!(
                "el proceso no es per-monitor DPI aware: con escalado distinto de 100 % \
                 las coordenadas físicas del monitor no coincidirán con las del escritorio \
                 virtual (llama a make_process_per_monitor_dpi_aware al arrancar)"
            );
        }
        Self::default()
    }

    pub fn pressed(&self) -> &PressedState {
        &self.pressed
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent, target_rect: ScreenRect) -> Result<(), InputError> {
        let inputs = build_inputs(event, target_rect, current_virtual_desktop())?;
        let result = send(&inputs);
        // Se registra también si el SO bloqueó parte: es preferible intentar
        // liberar de más al final que dejar algo pegado.
        self.pressed.record(event);
        result
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let desktop = current_virtual_desktop();
        let dummy = ScreenRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let mut inputs = Vec::new();
        for ev in self.pressed.release_all() {
            inputs.extend(build_inputs(&ev, dummy, desktop)?);
        }
        send(&inputs)
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        if !self.pressed.is_empty() {
            if let Err(e) = self.release_all() {
                tracing::warn!("no se pudo liberar la entrada pulsada al cerrar: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: VirtualDesktop = VirtualDesktop {
        x: -1366,
        y: 0,
        width: 1366 + 1920,
        height: 1080,
    };
    const PRIMARY: ScreenRect = ScreenRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn mi(i: &INPUT) -> MOUSEINPUT {
        assert_eq!(i.r#type, INPUT_MOUSE);
        // SAFETY: el tipo es INPUT_MOUSE, así que la variante activa es `mi`.
        unsafe { i.Anonymous.mi }
    }

    fn ki(i: &INPUT) -> KEYBDINPUT {
        assert_eq!(i.r#type, INPUT_KEYBOARD);
        // SAFETY: el tipo es INPUT_KEYBOARD, así que la variante activa es `ki`.
        unsafe { i.Anonymous.ki }
    }

    #[test]
    fn move_is_absolute_on_virtual_desktop_and_lands_on_pixel() {
        let ev = InputEvent::MouseMove {
            stream_id: 1,
            x_norm: 65535,
            y_norm: 0,
        };
        let v = build_inputs(&ev, PRIMARY, DESK).unwrap();
        assert_eq!(v.len(), 1);
        let m = mi(&v[0]);
        assert_eq!(
            m.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
        let (px, py) = DESK.from_absolute(m.dx as u16, m.dy as u16);
        assert_eq!((px, py), (1919, 0));
    }

    #[test]
    fn buttons_map_to_down_up_flags_and_xbuttons() {
        use ButtonState::*;
        let cases = [
            (MouseButton::Left, Pressed, MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, Released, MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, Pressed, MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Middle, Released, MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, Pressed, MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Forward, Released, MOUSEEVENTF_XUP, XBUTTON2),
        ];
        for (button, state, flags, data) in cases {
            let ev = InputEvent::MouseButton {
                stream_id: 1,
                button,
                state,
            };
            let v = build_inputs(&ev, PRIMARY, DESK).unwrap();
            let m = mi(&v[0]);
            assert_eq!(m.dwFlags, flags, "{button:?} {state:?}");
            assert_eq!(m.mouseData, data);
        }
    }

    #[test]
    fn wheel_vertical_and_horizontal() {
        let ev = InputEvent::MouseWheel {
            stream_id: 1,
            delta_x: -120,
            delta_y: 240,
        };
        let v = build_inputs(&ev, PRIMARY, DESK).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(mi(&v[0]).dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(mi(&v[0]).mouseData as i32, 240);
        assert_eq!(mi(&v[1]).dwFlags, MOUSEEVENTF_HWHEEL);
        assert_eq!(mi(&v[1]).mouseData as i32, -120);
    }

    #[test]
    fn keys_use_scancode_extended_and_keyup() {
        let ev = InputEvent::Key {
            stream_id: 1,
            scancode: 0x4B, // flecha izquierda con E0
            extended: true,
            state: ButtonState::Released,
        };
        let v = build_inputs(&ev, PRIMARY, DESK).unwrap();
        let k = ki(&v[0]);
        assert_eq!(k.wScan, 0x4B);
        assert_eq!(k.wVk, VIRTUAL_KEY(0));
        assert_eq!(
            k.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        let down = InputEvent::Key {
            stream_id: 1,
            scancode: 0x1E,
            extended: false,
            state: ButtonState::Pressed,
        };
        assert_eq!(
            ki(&build_inputs(&down, PRIMARY, DESK).unwrap()[0]).dwFlags,
            KEYEVENTF_SCANCODE
        );
    }

    #[test]
    fn invalid_events_and_rects_are_rejected_before_os() {
        let bad_key = InputEvent::Key {
            stream_id: 1,
            scancode: 0,
            extended: false,
            state: ButtonState::Pressed,
        };
        assert!(build_inputs(&bad_key, PRIMARY, DESK).is_err());
        let mv = InputEvent::MouseMove {
            stream_id: 1,
            x_norm: 1,
            y_norm: 1,
        };
        let zero = ScreenRect {
            width: 0,
            ..PRIMARY
        };
        match build_inputs(&mv, zero, DESK) {
            Err(e) => assert_eq!(e, InputError::InvalidRect(zero)),
            Ok(_) => panic!("debía rechazar un rectángulo de ancho 0"),
        }
    }
}
