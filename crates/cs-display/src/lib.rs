//! Monitores virtuales: descubrir los destinos virtuales que el driver expone,
//! mostrarlos (activarlos en modo extendido con un modo concreto) y retirarlos.
//!
//! Ver `docs/03-monitores-virtuales.md` y `docs/poc-results/poc-01-vdd.md`.
//! En Windows el driver (VDD) se preaprovisiona con N monitores y modos; aquí
//! solo se gestiona la topología con CCD, **sin privilegios**.

use cs_core::Mode;

#[cfg(windows)]
mod windows_ccd;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("no soportado en esta plataforma todavía")]
    Unsupported,
    #[error("llamada del sistema {call} falló con código {code}")]
    Os { call: &'static str, code: i64 },
    #[error(
        "no hay ningún monitor virtual disponible (¿driver instalado? ¿dispositivo en error?)"
    )]
    NoVirtualTarget,
    #[error("el monitor virtual no admite el modo {0:?}; hay que añadirlo a la configuración del driver")]
    ModeNotAvailable(Mode),
    #[error("el monitor virtual no apareció en la topología tras activarlo")]
    NotActivated,
    /// Windows niega cambiar la topología cuando la sesión está bloqueada o el
    /// proceso no tiene acceso a la consola interactiva (ERROR_ACCESS_DENIED).
    #[error("Windows no permite cambiar las pantallas ahora: la sesión está bloqueada o no es la consola activa. Desbloquea el equipo y vuelve a intentarlo")]
    SessionNotInteractive,
}

/// Un destino de pantalla (monitor) tal como lo ve el SO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTarget {
    /// Identificador estable dentro de la sesión del SO (adaptador + id de destino).
    pub key: TargetKey,
    pub friendly_name: String,
    pub device_path: String,
    pub is_virtual: bool,
    pub active: bool,
    /// Nombre GDI de la fuente (`\\.\DISPLAYn`) si está activo.
    pub gdi_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetKey {
    pub adapter_low: u32,
    pub adapter_high: i32,
    pub target_id: u32,
}

/// Monitor virtual mostrado en el escritorio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownDisplay {
    pub key: TargetKey,
    pub gdi_name: String,
    pub mode: Mode,
}

/// Patrón que identifica los monitores del driver VDD (EDID "MTT1337").
pub const VDD_DEVICE_PATH_MARKER: &str = "#MTT1337#";

/// Backend de topología. En Windows usa CCD; en otras plataformas aún no existe.
#[derive(Debug, Default)]
pub struct Displays {
    _private: (),
}

impl Displays {
    pub fn new() -> Self {
        Displays { _private: () }
    }

    /// Todos los destinos conectados (activos o no).
    pub fn targets(&self) -> Result<Vec<DisplayTarget>, DisplayError> {
        #[cfg(windows)]
        return windows_ccd::targets();
        #[cfg(not(windows))]
        Err(DisplayError::Unsupported)
    }

    /// Activa el primer monitor virtual inactivo (o el indicado) en modo
    /// extendido, a la derecha del escritorio actual, con el modo pedido.
    pub fn show(
        &self,
        target: Option<TargetKey>,
        mode: Mode,
    ) -> Result<ShownDisplay, DisplayError> {
        mode.validate()
            .map_err(|_| DisplayError::ModeNotAvailable(mode))?;
        #[cfg(windows)]
        return windows_ccd::show(target, mode);
        #[cfg(not(windows))]
        {
            let _ = target;
            Err(DisplayError::Unsupported)
        }
    }

    /// Retira (desactiva) el monitor virtual del escritorio sin tocar los demás.
    pub fn hide(&self, target: TargetKey) -> Result<(), DisplayError> {
        #[cfg(windows)]
        return windows_ccd::hide(target);
        #[cfg(not(windows))]
        {
            let _ = target;
            Err(DisplayError::Unsupported)
        }
    }

    /// Modos que el monitor activo `gdi_name` anuncia.
    pub fn modes(&self, gdi_name: &str) -> Result<Vec<Mode>, DisplayError> {
        #[cfg(windows)]
        return Ok(windows_ccd::modes(gdi_name));
        #[cfg(not(windows))]
        {
            let _ = gdi_name;
            Err(DisplayError::Unsupported)
        }
    }
}
