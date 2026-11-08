//! Consola: reactivar Ctrl+C.
//!
//! El atributo "ignorar Ctrl+C" de Windows se hereda de padres a hijos (lo
//! fijan algunos lanzadores y shells). Con él activo, `CTRL_C_EVENT` no llega
//! a ningún manejador y la sesión no podría parar de forma ordenada (ni
//! retirar el monitor virtual) al pulsar Ctrl+C. Detectado en las pruebas:
//! lanzado desde Git Bash, Ctrl+C no llegaba y Ctrl+Break sí.
#![allow(unsafe_code)] // FFI kernel32; un único bloque unsafe comentado.

use windows::Win32::System::Console::SetConsoleCtrlHandler;

/// Quita el atributo heredado "ignorar Ctrl+C" del proceso actual.
pub fn enable_ctrl_c() {
    // SAFETY: con manejador nulo y `FALSE`, la llamada solo restablece el
    // procesamiento normal de Ctrl+C del proceso; no registra punteros.
    if let Err(e) = unsafe { SetConsoleCtrlHandler(None, false) } {
        tracing::debug!("SetConsoleCtrlHandler(NULL, FALSE): {e}");
    }
}
