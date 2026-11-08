use serde::{Deserialize, Serialize};

/// Modo de vídeo: resolución en píxeles y frecuencia en milihercios
/// (59.94 Hz = 59_940) para representar frecuencias fraccionarias sin `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl Mode {
    /// Límites conservadores: lo que cualquier backend razonable debe rechazar.
    pub const MIN_DIM: u32 = 320;
    pub const MAX_DIM: u32 = 8192;
    pub const MIN_REFRESH_MHZ: u32 = 1_000;
    pub const MAX_REFRESH_MHZ: u32 = 500_000;

    pub const fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Mode {
            width,
            height,
            refresh_mhz: refresh_hz * 1000,
        }
    }

    /// Intervalo entre frames en microsegundos.
    pub fn frame_interval_us(&self) -> u64 {
        1_000_000_000 / u64::from(self.refresh_mhz.max(1))
    }

    pub fn validate(&self) -> Result<(), DisplaySpecError> {
        let dims_ok = (Self::MIN_DIM..=Self::MAX_DIM).contains(&self.width)
            && (Self::MIN_DIM..=Self::MAX_DIM).contains(&self.height);
        if !dims_ok {
            return Err(DisplaySpecError::Resolution(*self));
        }
        // H.264 y la mayoría de codificadores HW exigen dimensiones pares (4:2:0).
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(DisplaySpecError::OddDimensions(*self));
        }
        if !(Self::MIN_REFRESH_MHZ..=Self::MAX_REFRESH_MHZ).contains(&self.refresh_mhz) {
            return Err(DisplaySpecError::Refresh(*self));
        }
        Ok(())
    }
}

/// Descripción del monitor virtual que se pide crear en la fuente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySpec {
    /// Nombre visible en la configuración de pantalla del SO.
    pub name: String,
    /// Modo inicial; debe estar en `modes`.
    pub preferred: Mode,
    /// Modos que el monitor anuncia (equivalente a la lista de modos del EDID).
    pub modes: Vec<Mode>,
}

impl DisplaySpec {
    pub const MAX_NAME_LEN: usize = 13; // límite del descriptor de nombre en EDID

    pub fn single(name: impl Into<String>, mode: Mode) -> Self {
        DisplaySpec {
            name: name.into(),
            preferred: mode,
            modes: vec![mode],
        }
    }

    pub fn validate(&self) -> Result<(), DisplaySpecError> {
        if self.name.is_empty()
            || self.name.len() > Self::MAX_NAME_LEN
            || !self.name.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        {
            return Err(DisplaySpecError::Name);
        }
        if self.modes.is_empty() {
            return Err(DisplaySpecError::NoModes);
        }
        for m in &self.modes {
            m.validate()?;
        }
        if !self.modes.contains(&self.preferred) {
            return Err(DisplaySpecError::PreferredNotListed(self.preferred));
        }
        Ok(())
    }
}

/// Modo de operación de un flujo de pantalla.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamMode {
    /// La fuente crea un monitor virtual y lo transmite.
    Extend,
    /// La fuente transmite un monitor existente (sin driver).
    Duplicate,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DisplaySpecError {
    #[error("resolución fuera de rango: {0:?}")]
    Resolution(Mode),
    #[error("las dimensiones deben ser pares: {0:?}")]
    OddDimensions(Mode),
    #[error("frecuencia fuera de rango: {0:?}")]
    Refresh(Mode),
    #[error("nombre de monitor inválido (1-13 caracteres ASCII imprimibles)")]
    Name,
    #[error("el monitor no declara ningún modo")]
    NoModes,
    #[error("el modo preferido no está en la lista de modos: {0:?}")]
    PreferredNotListed(Mode),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_spec() {
        DisplaySpec::single("CrossScreen", Mode::new(1920, 1080, 60))
            .validate()
            .unwrap();
    }

    #[test]
    fn rejects_invalid() {
        let odd = Mode::new(1921, 1080, 60);
        assert_eq!(odd.validate(), Err(DisplaySpecError::OddDimensions(odd)));
        assert!(Mode::new(100, 100, 60).validate().is_err());
        assert!(Mode::new(1920, 1080, 0).validate().is_err());
        assert_eq!(
            DisplaySpec::single("", Mode::new(1920, 1080, 60)).validate(),
            Err(DisplaySpecError::Name)
        );
        let mut s = DisplaySpec::single("X", Mode::new(1920, 1080, 60));
        s.preferred = Mode::new(1280, 720, 60);
        assert!(matches!(
            s.validate(),
            Err(DisplaySpecError::PreferredNotListed(_))
        ));
    }

    #[test]
    fn frame_interval() {
        assert_eq!(Mode::new(1920, 1080, 60).frame_interval_us(), 16_666);
        let ntsc = Mode {
            width: 1920,
            height: 1080,
            refresh_mhz: 59_940,
        };
        assert_eq!(ntsc.frame_interval_us(), 16_683);
    }
}
