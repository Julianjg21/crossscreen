//! Guardia del monitor virtual: lo retira del escritorio al terminar la
//! sesión, **siempre** (salida normal, error, `?`, pánico con *unwind*).
//!
//! Lo que no puede cubrir ningún `Drop`: matar el proceso a la fuerza
//! (`TerminateProcess`, cerrar la consola sin manejador). Para Ctrl+C y el
//! cierre de consola la CLI convierte la señal en una parada ordenada.
//!
//! Varias sesiones a la vez (varios monitores, ADR-0009): cada guardia
//! **reclama** un destino virtual distinto en un registro del proceso, y los
//! cambios de topología se serializan con el mismo cerrojo.

use cs_core::Mode;
use cs_display::{DisplayError, DisplayTarget, Displays, ShownDisplay, TargetKey};
use std::sync::Mutex;

/// Destinos virtuales en uso por las guardias vivas de este proceso. El
/// cerrojo también serializa `show`/`hide` (CCD no admite cambios simultáneos).
static CLAIMED: Mutex<Vec<TargetKey>> = Mutex::new(Vec::new());

fn claimed() -> std::sync::MutexGuard<'static, Vec<TargetKey>> {
    CLAIMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Primer destino virtual que no use otra sesión de este proceso.
fn pick_free(targets: &[DisplayTarget], claimed: &[TargetKey]) -> Option<TargetKey> {
    targets
        .iter()
        .filter(|t| t.is_virtual && !claimed.contains(&t.key))
        .map(|t| t.key)
        .next()
}

/// Monitor virtual mostrado por esta sesión.
#[derive(Debug)]
pub struct VirtualDisplayGuard {
    displays: Displays,
    shown: Option<ShownDisplay>,
}

impl VirtualDisplayGuard {
    /// Muestra el primer modo de `candidates` que el driver admita.
    /// Solo pasa al siguiente candidato si el error es "modo no disponible".
    /// Con varias sesiones simultáneas, cada una recibe un destino virtual
    /// libre; si no queda ninguno devuelve [`DisplayError::NoVirtualTarget`].
    pub fn show_first(candidates: &[Mode]) -> Result<Self, DisplayError> {
        let displays = Displays::new();
        let mut claimed = claimed();
        let key = pick_free(&displays.targets()?, &claimed).ok_or_else(|| {
            tracing::warn!(
                en_uso = claimed.len(),
                "no queda ningún monitor virtual libre en el driver"
            );
            DisplayError::NoVirtualTarget
        })?;
        let mut last = DisplayError::NoVirtualTarget;
        for &m in candidates {
            match displays.show(Some(key), m) {
                Ok(shown) => {
                    tracing::info!(gdi = %shown.gdi_name, mode = ?shown.mode, "monitor virtual mostrado");
                    claimed.push(shown.key);
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
    let mut claimed = claimed();
    let r = d.hide(key);
    claimed.retain(|k| *k != key);
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

    fn target(id: u32, is_virtual: bool) -> DisplayTarget {
        DisplayTarget {
            key: TargetKey {
                adapter_low: 1,
                adapter_high: 0,
                target_id: id,
            },
            friendly_name: String::new(),
            device_path: String::new(),
            is_virtual,
            active: false,
            gdi_name: None,
        }
    }

    #[test]
    fn each_session_gets_a_distinct_virtual_target() {
        let ts = [target(1, false), target(2, true), target(3, true)];
        let a = pick_free(&ts, &[]).unwrap();
        assert_eq!(a.target_id, 2);
        let b = pick_free(&ts, &[a]).unwrap();
        assert_eq!(b.target_id, 3);
        assert_eq!(pick_free(&ts, &[a, b]), None);
        // Los monitores físicos nunca se eligen.
        assert_eq!(pick_free(&ts[..1], &[]), None);
    }

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
