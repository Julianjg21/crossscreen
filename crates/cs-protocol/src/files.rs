//! Transferencia de archivos (ADR-0010).
//!
//! Cada archivo viaja en su propio stream QUIC **unidireccional** (fiable y
//! cifrado como el resto de la conexión):
//!
//! ```text
//! "CSF1" | u16 LE longitud del nombre | nombre UTF-8 | u64 LE tamaño | bytes…
//! ```
//!
//! El emisor cierra el stream tras el último byte. Solo se abren streams de
//! archivo hacia un par que haya anunciado
//! [`ControlMessage::FilesAvailable`](crate::control::ControlMessage::FilesAvailable)
//! con `enabled: true`.
//!
//! El nombre recibido **no es de fiar**: [`sanitize_name`] lo reduce a un
//! nombre de archivo simple y válido en Windows y Linux antes de usarlo.

/// Marca de inicio de un stream de archivo.
pub const FILE_MAGIC: &[u8; 4] = b"CSF1";
/// Longitud máxima del nombre en el cable (bytes UTF-8).
pub const MAX_NAME_BYTES: usize = 1024;
/// Tamaño máximo de un archivo: 16 GiB.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Longitud máxima del nombre ya saneado (caracteres).
const MAX_SANE_CHARS: usize = 180;

/// Cabecera de un stream de archivo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FileHeaderError {
    #[error("no es un stream de archivo de CrossScreen")]
    BadMagic,
    #[error("nombre de archivo demasiado largo")]
    NameTooLong,
    #[error("nombre de archivo no válido (UTF-8)")]
    BadName,
    #[error("archivo demasiado grande ({0} bytes; máximo {MAX_FILE_BYTES})")]
    TooLarge(u64),
}

impl FileHeader {
    /// Serializa la cabecera.
    pub fn encode(&self) -> Result<Vec<u8>, FileHeaderError> {
        let name = self.name.as_bytes();
        if name.len() > MAX_NAME_BYTES {
            return Err(FileHeaderError::NameTooLong);
        }
        if self.size > MAX_FILE_BYTES {
            return Err(FileHeaderError::TooLarge(self.size));
        }
        let mut v = Vec::with_capacity(4 + 2 + name.len() + 8);
        v.extend_from_slice(FILE_MAGIC);
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(name);
        v.extend_from_slice(&self.size.to_le_bytes());
        Ok(v)
    }

    /// Valida el prefijo fijo (`magic` + longitud del nombre) y devuelve la
    /// longitud del nombre que sigue.
    pub fn decode_prefix(prefix: &[u8; 6]) -> Result<usize, FileHeaderError> {
        if &prefix[..4] != FILE_MAGIC {
            return Err(FileHeaderError::BadMagic);
        }
        let n = u16::from_le_bytes([prefix[4], prefix[5]]) as usize;
        if n > MAX_NAME_BYTES {
            return Err(FileHeaderError::NameTooLong);
        }
        Ok(n)
    }

    /// Completa la cabecera con el nombre y los 8 bytes del tamaño.
    pub fn decode_rest(name: &[u8], size: [u8; 8]) -> Result<Self, FileHeaderError> {
        let name = std::str::from_utf8(name)
            .map_err(|_| FileHeaderError::BadName)?
            .to_owned();
        let size = u64::from_le_bytes(size);
        if size > MAX_FILE_BYTES {
            return Err(FileHeaderError::TooLarge(size));
        }
        Ok(Self { name, size })
    }
}

/// Reduce un nombre recibido a un nombre de archivo simple y seguro:
/// - solo el último componente (sin rutas ni `..`);
/// - sin caracteres de control ni los prohibidos en Windows (`<>:"/\|?*`);
/// - sin puntos ni espacios al final, ni nombres reservados de Windows
///   (`CON`, `NUL`, `COM1`…);
/// - con un máximo de 180 caracteres, conservando la extensión.
///
/// Devuelve `None` si no queda nada utilizable.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = last
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    let s_trim = s.trim_start();
    if s_trim.is_empty() || s_trim.chars().all(|c| c == '.' || c == '_') {
        return None;
    }
    let mut s = s_trim.to_owned();
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let reserved = RESERVED.contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    if s.chars().count() > MAX_SANE_CHARS {
        let (stem, ext) = match s.rfind('.') {
            Some(i) if i > 0 && s.len() - i <= 16 => (&s[..i], &s[i..]),
            _ => (s.as_str(), ""),
        };
        let keep = MAX_SANE_CHARS - ext.chars().count();
        s = stem.chars().take(keep).collect::<String>() + ext;
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = FileHeader {
            name: "informe ñ.pdf".into(),
            size: 123_456_789,
        };
        let b = h.encode().unwrap();
        let n = FileHeader::decode_prefix(b[..6].try_into().unwrap()).unwrap();
        let name = &b[6..6 + n];
        let size: [u8; 8] = b[6 + n..].try_into().unwrap();
        assert_eq!(FileHeader::decode_rest(name, size).unwrap(), h);
    }

    #[test]
    fn header_rejects_garbage_and_limits() {
        assert_eq!(
            FileHeader::decode_prefix(b"HTTP/1"),
            Err(FileHeaderError::BadMagic)
        );
        let mut p = *b"CSF1\0\0";
        p[4..].copy_from_slice(&((MAX_NAME_BYTES + 1) as u16).to_le_bytes());
        assert_eq!(
            FileHeader::decode_prefix(&p),
            Err(FileHeaderError::NameTooLong)
        );
        assert!(matches!(
            FileHeader::decode_rest(b"a", (MAX_FILE_BYTES + 1).to_le_bytes()),
            Err(FileHeaderError::TooLarge(_))
        ));
        assert_eq!(
            FileHeader::decode_rest(&[0xff, 0xfe], 1u64.to_le_bytes()),
            Err(FileHeaderError::BadName)
        );
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("foto.png").as_deref(), Some("foto.png"));
        assert_eq!(sanitize_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            sanitize_name("C:\\Windows\\System32\\evil.dll").as_deref(),
            Some("evil.dll")
        );
        assert_eq!(
            sanitize_name("a<b>c:d?.txt").as_deref(),
            Some("a_b_c_d_.txt")
        );
        assert_eq!(sanitize_name("nota.txt. . ").as_deref(), Some("nota.txt"));
        assert_eq!(sanitize_name("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(sanitize_name("COM3").as_deref(), Some("_COM3"));
        assert_eq!(sanitize_name("COMMA.txt").as_deref(), Some("COMMA.txt"));
        assert_eq!(sanitize_name("x\u{1b}[2J.sh").as_deref(), Some("x_[2J.sh"));
        assert_eq!(sanitize_name(".."), None);
        assert_eq!(sanitize_name("dir/"), None);
        assert_eq!(sanitize_name("   "), None);
        let long = format!("{}.tar.gz", "a".repeat(400));
        let s = sanitize_name(&long).unwrap();
        assert_eq!(s.chars().count(), 180);
        assert!(s.ends_with(".gz"));
    }
}
