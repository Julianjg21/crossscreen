//! Portapapeles de texto en Linux con `arboard` (X11; en GNOME Wayland a
//! través de XWayland, que Mutter mantiene sincronizado con el portapapeles
//! de Wayland).
//!
//! X11 no tiene un contador de cambios como `GetClipboardSequenceNumber`:
//! [`LinuxClipboard::sequence_number`] lee el texto y avanza un contador
//! propio cuando su huella cambia. El objeto debe vivir mientras se quiera
//! ofrecer lo escrito (en X11 el dueño de la selección es este proceso).

use crate::{hash_text, text, ClipboardError, TextHash};
use std::cell::{Cell, RefCell};

pub struct LinuxClipboard {
    inner: RefCell<arboard::Clipboard>,
    seq: Cell<u32>,
    /// Huella y texto de la última lectura (o escritura propia).
    last: RefCell<Option<(TextHash, String)>>,
    /// ¿Ya hubo una lectura válida? Mientras no, el contenido es desconocido
    /// y la primera lectura que funcione solo fija la línea base (no cuenta
    /// como cambio): así lo copiado antes de la sesión nunca se envía aunque
    /// la lectura inicial fallara (sin *fail-open*).
    known: Cell<bool>,
}

impl std::fmt::Debug for LinuxClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxClipboard")
            .field("seq", &self.seq.get())
            .finish_non_exhaustive()
    }
}

fn os_error(call: &'static str, e: &arboard::Error) -> ClipboardError {
    tracing::debug!("{call}: {e}");
    match e {
        arboard::Error::ClipboardOccupied => ClipboardError::Busy,
        _ => ClipboardError::Os { call, code: 0 },
    }
}

impl LinuxClipboard {
    /// Conecta con el portapapeles (necesita `DISPLAY`: X11 o XWayland).
    pub fn new() -> Result<Self, ClipboardError> {
        let cb = arboard::Clipboard::new().map_err(|e| {
            tracing::warn!("portapapeles de Linux no disponible: {e}");
            ClipboardError::Unsupported
        })?;
        let me = Self {
            inner: RefCell::new(cb),
            seq: Cell::new(1),
            last: RefCell::new(None),
            known: Cell::new(false),
        };
        // Estado inicial sin avanzar el contador.
        let _ = me.poll();
        Ok(me)
    }

    /// Lee el texto actual (canónico) y actualiza la huella; `true` si cambió.
    fn poll(&self) -> Result<bool, ClipboardError> {
        let text = match self.inner.borrow_mut().get_text() {
            Ok(t) => Some(text::canonicalize(&t)),
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(e) => return Err(os_error("get_text", &e)),
        };
        let now = text.filter(|t| !t.is_empty()).map(|t| (hash_text(&t), t));
        let mut last = self.last.borrow_mut();
        if !self.known.replace(true) {
            // Primera lectura válida: línea base, no un cambio.
            *last = now;
            return Ok(false);
        }
        let changed = last.as_ref().map(|(h, _)| h) != now.as_ref().map(|(h, _)| h);
        if changed {
            *last = now;
        }
        Ok(changed)
    }

    /// Contador que avanza cuando cambia el texto del portapapeles.
    pub fn sequence_number(&self) -> u32 {
        if let Ok(true) = self.poll() {
            self.seq.set(self.seq.get().wrapping_add(1));
        }
        self.seq.get()
    }

    /// Texto actual (canónico) o `None` si no hay texto.
    pub fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        self.poll()?;
        Ok(self.last.borrow().as_ref().map(|(_, t)| t.clone()))
    }

    /// Escribe `canonical` y devuelve el contador tras la escritura (para no
    /// devolverlo como cambio local: sin eco).
    pub fn write_text(&self, canonical: &str) -> Result<u32, ClipboardError> {
        crate::validate_text(canonical)?;
        self.inner
            .borrow_mut()
            .set_text(canonical.to_owned())
            .map_err(|e| os_error("set_text", &e))?;
        *self.last.borrow_mut() = Some((hash_text(canonical), canonical.to_owned()));
        self.known.set(true);
        self.seq.set(self.seq.get().wrapping_add(1));
        Ok(self.seq.get())
    }
}
