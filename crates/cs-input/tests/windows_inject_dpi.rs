//! Igual que `windows_inject.rs`, pero en un proceso *per-monitor DPI aware*:
//! es el modo del emisor real, donde los rectángulos de DXGI/CCD están en
//! píxeles físicos. Binario aparte porque el modo DPI es global al proceso.
//! `cargo test -p cs-input --test windows_inject_dpi -- --ignored`.
#![cfg(windows)]
#![allow(unsafe_code)] // FFI Win32 mínima para leer y restaurar el cursor.

use cs_input::{
    current_virtual_desktop, is_per_monitor_dpi_aware, make_process_per_monitor_dpi_aware,
    InputEvent, InputInjector, ScreenRect, SendInputInjector,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: puntero a un POINT válido en la pila.
    unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
    (p.x, p.y)
}

fn move_to(inj: &mut SendInputInjector, x: i32, y: i32) {
    let rect = ScreenRect {
        x,
        y,
        width: 1,
        height: 1,
    };
    let ev = InputEvent::MouseMove {
        stream_id: 1,
        x_norm: 0,
        y_norm: 65535,
    };
    inj.inject(&ev, rect).expect("SendInput");
    std::thread::sleep(std::time::Duration::from_millis(50));
}

#[test]
#[ignore = "mueve el cursor real; requiere sesión interactiva"]
fn absolute_move_lands_on_exact_physical_pixel() {
    make_process_per_monitor_dpi_aware().expect("activar DPI awareness");
    assert!(is_per_monitor_dpi_aware());
    let desk = current_virtual_desktop();
    eprintln!("escritorio virtual (píxeles físicos): {desk:?}");
    let original = cursor();
    let mut inj = SendInputInjector::new();
    let dx = if original.0 > desk.x { -1 } else { 1 };
    let dy = if original.1 > desk.y { -1 } else { 1 };
    let target = (original.0 + dx, original.1 + dy);
    move_to(&mut inj, target.0, target.1);
    let got = cursor();
    move_to(&mut inj, original.0, original.1);
    let back = cursor();
    // SAFETY: sin punteros.
    unsafe {
        let _ = SetCursorPos(original.0, original.1);
    }
    assert_eq!(got, target, "no cayó en el píxel físico pedido");
    assert_eq!(back, original);
}
