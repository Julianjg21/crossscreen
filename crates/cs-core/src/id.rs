use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Identificador público y estable de un dispositivo.
///
/// Se deriva de la huella SHA-256 del certificado del dispositivo (ver
/// `cs-security`): son los primeros 16 bytes. No es secreto; se anuncia por
/// mDNS. La autenticación real se hace con la huella completa fijada.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 16]);

impl DeviceId {
    pub fn from_fingerprint(fp: &[u8; 32]) -> Self {
        let mut id = [0u8; 16];
        id.copy_from_slice(&fp[..16]);
        DeviceId(id)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("identificador de dispositivo inválido: se esperan 32 caracteres hexadecimales")]
pub struct DeviceIdParseError;

impl FromStr for DeviceId {
    type Err = DeviceIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|_| DeviceIdParseError)?;
        let arr: [u8; 16] = bytes.try_into().map_err(|_| DeviceIdParseError)?;
        Ok(DeviceId(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn display_parse_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = DeviceId(bytes);
            prop_assert_eq!(id.to_string().parse::<DeviceId>().unwrap(), id);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!("zz".parse::<DeviceId>().is_err());
        assert!("00".parse::<DeviceId>().is_err());
    }
}
