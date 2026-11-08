//! Portapapeles de Windows: texto `CF_UNICODETEXT`.
//!
//! - `GetClipboardSequenceNumber` permite saber si el portapapeles cambió sin
//!   abrirlo (el sondeo es barato).
//! - Se abre con una **ventana propia de solo mensajes** como propietaria: con
//!   `OpenClipboard(NULL)`, `EmptyClipboard` deja el propietario a nulo y la
//!   documentación advierte que `SetClipboardData` puede fallar.
//! - [`OpenGuard`] garantiza `CloseClipboard` en cualquier salida, y la
//!   apertura se reintenta brevemente si otro proceso lo tiene abierto.
//!
//! El objeto no es `Send` (la ventana pertenece al hilo que la crea): úsalo
//! siempre desde el mismo hilo.
#![allow(unsafe_code)] // FFI Win32; cada bloque unsafe está acotado y comentado.

use crate::{text, ClipboardError};
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// Formato de texto Unicode (winuser.h).
const CF_UNICODETEXT: u32 = 13;
/// Formatos que Windows sintetiza a partir de `CF_UNICODETEXT`.
const CF_TEXT: u32 = 1;
const CF_OEMTEXT: u32 = 7;
const CF_LOCALE: u32 = 16;

const OPEN_RETRIES: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(15);

fn last_error(call: &'static str) -> ClipboardError {
    // SAFETY: sin argumentos; lee el último error del hilo.
    let code = unsafe { GetLastError() };
    ClipboardError::Os {
        call,
        code: i64::from(code.0),
    }
}

/// Número de secuencia del portapapeles: cambia con cada modificación.
pub fn sequence_number() -> u32 {
    // SAFETY: sin argumentos ni efectos secundarios.
    unsafe { GetClipboardSequenceNumber() }
}

/// Portapapeles abierto; lo cierra al soltarse.
struct OpenGuard;

impl OpenGuard {
    fn open(owner: HWND) -> Result<Self, ClipboardError> {
        for attempt in 0..OPEN_RETRIES {
            // SAFETY: `owner` es una ventana válida de este hilo (o nula).
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(OpenGuard);
            }
            if attempt + 1 < OPEN_RETRIES {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        Err(ClipboardError::Busy)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: este hilo abrió el portapapeles en `open`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Bloqueo de un HGLOBAL; lo desbloquea al soltarse.
struct LockGuard {
    mem: HGLOBAL,
    ptr: *mut core::ffi::c_void,
}

impl LockGuard {
    fn lock(mem: HGLOBAL) -> Option<Self> {
        // SAFETY: `mem` es un HGLOBAL válido (de GlobalAlloc o del portapapeles).
        let ptr = unsafe { GlobalLock(mem) };
        (!ptr.is_null()).then_some(LockGuard { mem, ptr })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // SAFETY: bloqueado en `lock`; GlobalUnlock "falla" con error 0 cuando
        // el contador llega a cero, lo que es normal.
        let _ = unsafe { GlobalUnlock(self.mem) };
    }
}

/// Acceso al portapapeles de texto de Windows.
#[derive(Debug)]
pub struct Win32Clipboard {
    owner: HWND,
}

impl Win32Clipboard {
    pub fn new() -> Result<Self, ClipboardError> {
        // SAFETY: clase de sistema "STATIC"; ventana de solo mensajes, sin
        // parámetros propios. Se destruye en Drop.
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("CrossScreen clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
        }
        .map_err(|e| ClipboardError::Os {
            call: "CreateWindowExW",
            code: i64::from(e.code().0),
        })?;
        Ok(Win32Clipboard { owner })
    }

    /// Número de secuencia actual (ver [`sequence_number`]).
    pub fn sequence_number(&self) -> u32 {
        sequence_number()
    }

    /// `true` si el portapapeles contiene formatos además del texto (imagen,
    /// HTML, RTF, archivos…). Útil para no destruir contenido rico al escribir.
    pub fn has_non_text_formats(&self) -> Result<bool, ClipboardError> {
        let _g = OpenGuard::open(self.owner)?;
        // SAFETY: portapapeles abierto por este hilo.
        let total = unsafe { CountClipboardFormats() };
        let text_formats = [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT, CF_LOCALE]
            .iter()
            // SAFETY: consulta sin punteros.
            .filter(|f| unsafe { IsClipboardFormatAvailable(**f) }.is_ok())
            .count() as i32;
        Ok(total > text_formats)
    }

    /// Lee el texto del portapapeles (`None` si no hay texto). Devuelve el
    /// texto tal cual (CRLF incluido); usa `text::canonicalize` para comparar.
    pub fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        // SAFETY: consulta sin punteros; no requiere abrir el portapapeles.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
            return Ok(None);
        }
        let _g = OpenGuard::open(self.owner)?;
        // SAFETY: portapapeles abierto; el handle pertenece al portapapeles y
        // es válido hasta CloseClipboard (no se libera aquí).
        let handle = match unsafe { GetClipboardData(CF_UNICODETEXT) } {
            Ok(h) if !h.is_invalid() => h,
            _ => return Ok(None),
        };
        let mem = HGLOBAL(handle.0);
        let Some(lock) = LockGuard::lock(mem) else {
            return Err(last_error("GlobalLock"));
        };
        // SAFETY: tamaño en bytes del bloque bloqueado.
        let bytes = unsafe { GlobalSize(mem) };
        let units = bytes / 2;
        // SAFETY: `lock.ptr` apunta a `bytes` bytes válidos mientras dure el
        // bloqueo; se lee como u16 (alineación garantizada por GlobalAlloc).
        let slice = unsafe { std::slice::from_raw_parts(lock.ptr as *const u16, units) };
        Ok(Some(text::from_utf16_nul(slice)))
    }

    /// Escribe texto canónico (`\n`) en el portapapeles, convertido a CRLF.
    /// Sustituye todo el contenido. Devuelve el número de secuencia posterior.
    pub fn write_text(&self, canonical: &str) -> Result<u32, ClipboardError> {
        crate::validate_text(canonical)?;
        let wide = text::to_utf16_nul(&text::to_windows_newlines(canonical));
        let bytes = wide.len() * 2;
        // SAFETY: reserva de memoria movible; se libera abajo si no la toma el SO.
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|e| ClipboardError::Os {
            call: "GlobalAlloc",
            code: i64::from(e.code().0),
        })?;
        {
            let Some(lock) = LockGuard::lock(mem) else {
                // SAFETY: `mem` es nuestro y no se ha entregado al SO.
                let _ = unsafe { GlobalFree(mem) };
                return Err(last_error("GlobalLock"));
            };
            // SAFETY: el bloque tiene `bytes` bytes; origen y destino no se solapan.
            unsafe {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), lock.ptr as *mut u16, wide.len());
            }
        }
        let result = (|| {
            let _g = OpenGuard::open(self.owner)?;
            // SAFETY: portapapeles abierto por este hilo con propietario propio.
            unsafe { EmptyClipboard() }.map_err(|_| last_error("EmptyClipboard"))?;
            // SAFETY: tras el éxito, el SO es dueño de `mem`.
            unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE(mem.0)) }
                .map_err(|_| last_error("SetClipboardData"))?;
            Ok(())
        })();
        if result.is_err() {
            // SAFETY: SetClipboardData no llegó a tomar la memoria.
            let _ = unsafe { GlobalFree(mem) };
        }
        result.map(|()| sequence_number())
    }
}

impl Win32Clipboard {
    /// Vacía el portapapeles. Devuelve el número de secuencia posterior.
    pub fn clear(&self) -> Result<u32, ClipboardError> {
        let _g = OpenGuard::open(self.owner)?;
        // SAFETY: portapapeles abierto por este hilo con propietario propio.
        unsafe { EmptyClipboard() }.map_err(|_| last_error("EmptyClipboard"))?;
        drop(_g);
        Ok(sequence_number())
    }
}

impl Drop for Win32Clipboard {
    fn drop(&mut self) {
        // SAFETY: ventana creada en `new` por este mismo objeto.
        let _ = unsafe { DestroyWindow(self.owner) };
    }
}
