//! Verificación contra el Windows real de la conversión de coordenadas.
//! Mueve el cursor del usuario 1 px y lo devuelve a su sitio; no pulsa teclas
//! ni botones. Ignorado por defecto (necesita una sesión interactiva):
//! `cargo test -p cs-input --test windows_inject -- --ignored`.
#![cfg(windows)]
#![allow(unsafe_code)] // FFI Win32 mínima para leer y restaurar el cursor.

use cs_input::{InputEvent, InputInjector, ScreenRect, SendInputInjector};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: puntero a un POINT válido en la pila.
    unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
    (p.x, p.y)
}

/// Inyecta un movimiento absoluto al píxel exacto `(x, y)` usando un
/// rectángulo de 1×1 (cualquier valor normalizado cae en ese píxel).
fn move_to(inj: &mut SendInputInjector, x: i32, y: i32) {
    let rect = ScreenRect {
        x,
        y,
        width: 1,
        height: 1,
    };
    let ev = InputEvent::MouseMove {
        stream_id: 1,
        x_norm: 32768,
        y_norm: 32768,
    };
    inj.inject(&ev, rect).expect("SendInput");
    std::thread::sleep(std::time::Duration::from_millis(50));
}

#[test]
#[ignore = "mueve el cursor real; requiere sesión interactiva"]
fn absolute_move_lands_on_exact_pixel_on_real_windows() {
    let original = cursor();
    let mut inj = SendInputInjector::new();
    // Un píxel a la derecha y abajo (o a la izquierda/arriba si está en el borde).
    let dx = if original.0 > 0 { -1 } else { 1 };
    let dy = if original.1 > 0 { -1 } else { 1 };
    let target = (original.0 + dx, original.1 + dy);
    move_to(&mut inj, target.0, target.1);
    let got = cursor();
    move_to(&mut inj, original.0, original.1);
    let back = cursor();
    // Restaurar pase lo que pase.
    // SAFETY: sin punteros.
    unsafe {
        let _ = SetCursorPos(original.0, original.1);
    }
    assert_eq!(
        got, target,
        "el movimiento absoluto no cayó en el píxel pedido"
    );
    assert_eq!(back, original, "no volvió a la posición original");
    assert!(inj.pressed().is_empty());
}
