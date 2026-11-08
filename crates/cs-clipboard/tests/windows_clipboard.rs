//! Pruebas contra el portapapeles real de Windows.
//!
//! Tocan el portapapeles del usuario: se omiten si contiene algo más que texto
//! (para no destruir imágenes, HTML, etc.) y restauran el texto original al
//! terminar. Ignoradas por defecto:
//! `cargo test -p cs-clipboard --test windows_clipboard -- --ignored --test-threads=1`.
#![cfg(windows)]
#![allow(unsafe_code)] // FFI mínima para simular otro proceso con el portapapeles abierto.

use cs_clipboard::text::canonicalize;
use cs_clipboard::{ClipboardError, ClipboardSync, ClipboardText, RemoteAction, Win32Clipboard};
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

/// Ejecuta `f` con el portapapeles preservado; `None` si se omite.
fn with_preserved_clipboard(f: impl FnOnce(&Win32Clipboard)) -> Option<()> {
    let cb = Win32Clipboard::new().expect("ventana propietaria");
    if cb.has_non_text_formats().expect("consultar formatos") {
        eprintln!("OMITIDO: el portapapeles tiene contenido no textual; no se toca");
        return None;
    }
    let original = cb.read_text().expect("leer original");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&cb)));
    match &original {
        Some(t) if !t.is_empty() => {
            cb.write_text(&canonicalize(t)).expect("restaurar texto");
        }
        _ => {
            cb.clear().expect("restaurar vacío");
        }
    }
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
    Some(())
}

#[test]
#[ignore = "usa el portapapeles real del usuario (solo lectura)"]
fn read_is_non_destructive_and_does_not_change_sequence() {
    let cb = Win32Clipboard::new().expect("ventana propietaria");
    let before = cb.sequence_number();
    let first = cb.read_text().expect("leer");
    let second = cb.read_text().expect("leer de nuevo");
    assert_eq!(first, second, "dos lecturas seguidas dan lo mismo");
    assert_eq!(
        cb.sequence_number(),
        before,
        "leer no modifica el portapapeles"
    );
    let _ = cb.has_non_text_formats().expect("consultar formatos");
    eprintln!(
        "lectura real OK: {} caracteres de texto, secuencia {before}",
        first.as_deref().map_or(0, |t| t.chars().count())
    );
}

#[test]
#[ignore = "usa el portapapeles real del usuario"]
fn write_read_roundtrip_unicode_and_crlf() {
    with_preserved_clipboard(|cb| {
        let canonical = "CrossScreen 👍🏽 漢字\nlínea 2\n\nfin";
        let before = cb.sequence_number();
        let after = cb.write_text(canonical).expect("escribir");
        assert_ne!(before, after, "el número de secuencia debe cambiar");
        let raw = cb.read_text().expect("leer").expect("hay texto");
        assert!(raw.contains("\r\n"), "en Windows se guarda con CRLF");
        assert_eq!(canonicalize(&raw), canonical);
    });
}

#[test]
#[ignore = "usa el portapapeles real del usuario"]
fn remote_text_applied_to_os_is_not_echoed_back() {
    with_preserved_clipboard(|cb| {
        let mut sync = ClipboardSync::new();
        // Estado inicial: leer lo que haya para fijar la secuencia vista.
        let s0 = cb.sequence_number();
        if sync.should_read(s0) {
            let _ = sync.on_local_change(cb.read_text().unwrap().as_deref());
        }
        let msg = ClipboardText {
            seq: 1,
            text: "texto del par remoto\ncon salto".into(),
        };
        let RemoteAction::Apply(text) = sync.on_remote(&msg).unwrap() else {
            panic!("debía aplicarse");
        };
        let seq_after = cb.write_text(&text).expect("escribir");
        sync.after_local_write(&text, seq_after);
        // Sondeo: el cambio es nuestro → no se lee ni se reenvía.
        assert!(!sync.should_read(cb.sequence_number()));
        // Aunque se forzara la lectura, el contenido coincide → nada que enviar.
        assert!(sync
            .on_local_change(cb.read_text().unwrap().as_deref())
            .is_none());
        assert_eq!(sync.stats().echoes_suppressed, 1);
    });
}

/// Abre el portapapeles desde otro hilo durante `hold` (simula otro proceso).
fn hold_clipboard_from_other_thread(hold: Duration) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        // SAFETY: apertura sin propietario desde este hilo; se cierra abajo.
        let opened = unsafe { OpenClipboard(HWND::default()) }.is_ok();
        tx.send(opened).unwrap();
        std::thread::sleep(hold);
        if opened {
            // SAFETY: este hilo abrió el portapapeles.
            let _ = unsafe { CloseClipboard() };
        }
    });
    assert!(
        rx.recv().unwrap(),
        "el hilo auxiliar no pudo abrir el portapapeles"
    );
    h
}

#[test]
#[ignore = "usa el portapapeles real del usuario"]
fn busy_clipboard_is_retried_then_reported() {
    with_preserved_clipboard(|cb| {
        // Ocupado poco tiempo: los reintentos lo absorben.
        let h = hold_clipboard_from_other_thread(Duration::from_millis(40));
        cb.write_text("tras reintento")
            .expect("debía reintentar y escribir");
        h.join().unwrap();
        assert_eq!(cb.read_text().unwrap().as_deref(), Some("tras reintento"));
        // Ocupado más que la ventana de reintentos (~150 ms): error claro.
        let h = hold_clipboard_from_other_thread(Duration::from_millis(600));
        assert_eq!(cb.write_text("no cabe"), Err(ClipboardError::Busy));
        h.join().unwrap();
    });
}
