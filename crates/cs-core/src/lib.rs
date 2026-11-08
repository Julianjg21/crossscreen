//! Tipos de dominio compartidos por todos los crates de CrossScreen.
//!
//! Este crate no depende de ningún sistema operativo ni de I/O: solo define
//! vocabulario (identificadores, modos de pantalla, códecs, roles) y sus
//! invariantes.

mod display;
mod id;
mod media;

pub use display::{DisplaySpec, DisplaySpecError, Mode, StreamMode};
pub use id::{DeviceId, DeviceIdParseError};
pub use media::{Codec, CodecSupport, H264Profile};

use serde::{Deserialize, Serialize};

/// Rol que un equipo desempeña dentro de una sesión.
///
/// Ver la terminología en `README.md`: la *fuente* extiende o duplica su
/// escritorio; el *receptor* actúa como pantalla.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Source,
    Sink,
}

/// Conjunto de roles que un equipo admite (anunciado por mDNS y en el handshake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Roles {
    pub source: bool,
    pub sink: bool,
}

impl Roles {
    pub const BOTH: Roles = Roles {
        source: true,
        sink: true,
    };

    pub fn supports(self, role: Role) -> bool {
        match role {
            Role::Source => self.source,
            Role::Sink => self.sink,
        }
    }

    /// Codificación compacta para registros TXT de mDNS: "s", "k", "sk" o "".
    pub fn to_txt(self) -> String {
        let mut s = String::new();
        if self.source {
            s.push('s');
        }
        if self.sink {
            s.push('k');
        }
        s
    }

    pub fn from_txt(s: &str) -> Roles {
        Roles {
            source: s.contains('s'),
            sink: s.contains('k'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_txt_roundtrip() {
        for r in [
            Roles::default(),
            Roles {
                source: true,
                sink: false,
            },
            Roles {
                source: false,
                sink: true,
            },
            Roles::BOTH,
        ] {
            assert_eq!(Roles::from_txt(&r.to_txt()), r);
        }
    }
}
