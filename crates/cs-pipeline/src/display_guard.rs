//! Guardia del monitor virtual: lo retira del escritorio al terminar la
//! sesión, **siempre** (salida normal, error, `?`, pánico con *unwind*).
//!
//! Lo que no puede cubrir ningún `Drop`: matar el proceso a la fuerza
//! (`TerminateProcess`, cerrar la consola sin manejador). Para Ctrl+C y el
//! cierre de consola la CLI convierte la señal en una parada ordenada.

use cs_core::Mode;
use cs_display::{DisplayError, Displays, ShownDisplay, TargetKey};

/// Monitor virtual mostrado por esta sesión.
#[derive(Debug)]
pub struct VirtualDisplayGuard {
    displays: Displays,
    shown: Option<ShownDisplay>,
}

impl VirtualDisplayGuard {
    /// Muestra el primer modo de `candidates` que el driver admita.
    /// Solo pasa al siguiente candidato si el error es "modo no disponible".
    pub fn show_first(candidates: &[Mode]) -> Result<Self, DisplayError> {
        let displays = Displays::new();
        let mut last = DisplayError::NoVirtualTarget;
        for &m in candidates {
            match displays.show(None, m) {
                Ok(shown) => {
                    tracing::info!(gdi = %shown.gdi_name, mode = ?shown.mode, "monitor virtual mostrado");
                    return Ok(Self {
                        displays,
                        shown: Some(shown),
                    });
                }
                Err(e @ DisplayError::ModeNotAvailable(_)) => {
                    tracing::info!("modo {m:?} no disponible en el driver: {e}");
                    last = e;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// Nombre GDI y modo aplicado.
    pub fn shown(&self) -> Option<&ShownDisplay> {
        self.shown.as_ref()
    }

    /// Retira el monitor ahora y devuelve el resultado (el `Drop` ya no hará nada).
    pub fn hide(mut self) -> Result<(), DisplayError> {
        match self.shown.take() {
            Some(s) => hide_key(&self.displays, s.key),
            None => Ok(()),
        }
    }
}

fn hide_key(d: &Displays, key: TargetKey) -> Result<(), DisplayError> {
    let r = d.hide(key);
    match &r {
        Ok(()) => tracing::info!("monitor virtual retirado"),
        Err(e) => tracing::error!("no se pudo retirar el monitor virtual: {e}"),
    }
    r
}

impl Drop for VirtualDisplayGuard {
    fn drop(&mut self) {
        if let Some(s) = self.shown.take() {
            if let Err(e) = hide_key(&self.displays, s.key) {
                eprintln!(
                    "AVISO: no se pudo retirar el monitor virtual {} ({e}); usa `crossscreen display hide`",
                    s.gdi_name
                );
            }
        }
    }
}

/// Modos a probar, en orden: el pedido por CLI si lo hay; si no, el nativo
/// del receptor, el nativo a 60 Hz y el de reserva.
pub fn mode_candidates(requested: Option<Mode>, native: Mode, fallback: Mode) -> Vec<Mode> {
    let mut v = match requested {
        Some(m) => vec![m],
        None => vec![native, Mode::new(native.width, native.height, 60), fallback],
    };
    let mut seen = Vec::new();
    v.retain(|m| {
        let dup = seen.contains(m);
        seen.push(*m);
        !dup && m.validate().is_ok()
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_cli_then_native() {
        let fb = Mode::new(1920, 1080, 60);
        let native = Mode::new(2560, 1600, 120);
        assert_eq!(
            mode_candidates(Some(Mode::new(1280, 720, 60)), native, fb),
            vec![Mode::new(1280, 720, 60)]
        );
        assert_eq!(
            mode_candidates(None, native, fb),
            vec![native, Mode::new(2560, 1600, 60), fb]
        );
        // Sin duplicados.
        assert_eq!(mode_candidates(None, fb, fb), vec![fb]);
        // Un nativo inválido (impar) se descarta.
        let odd = Mode::new(1365, 768, 60);
        assert_eq!(mode_candidates(None, odd, fb), vec![fb]);
    }
}
