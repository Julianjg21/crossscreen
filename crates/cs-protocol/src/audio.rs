//! Datagramas de audio (ADR-0008).
//!
//! Cada datagrama lleva **una** trama Opus completa (sin fragmentar):
//!
//! ```text
//!  0       1       2               4                               8
//! +-------+-------+---------------+-------------------------------+
//! | kind  | flags |   stream_id   |            seq                |
//! +-------+-------+---------------+-------------------------------+
//! |        pts (muestras a 48 kHz)|  carga Opus …                 |
//! +-------------------------------+
//! ```
//!
//! `kind` = [`AUDIO_KIND`] (los datagramas de vídeo empiezan por
//! [`crate::video::WIRE_VERSION`] = 1), así el receptor los distingue por el
//! primer byte. Enteros en *big endian*.

/// Primer byte de un datagrama de audio.
pub const AUDIO_KIND: u8 = 2;
/// Longitud de la cabecera de audio.
pub const AUDIO_HEADER_LEN: usize = 12;
/// Tope de la carga Opus (una trama de 10–20 ms a la tasa máxima de Opus
/// ocupa como mucho ~1,3 KB; 1275 B es el máximo por trama de la RFC 6716).
pub const MAX_AUDIO_PAYLOAD: usize = 1275;

/// Cabecera de un datagrama de audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub stream_id: u16,
    /// Número de secuencia de la trama (crece de uno en uno; da la vuelta).
    pub seq: u32,
    /// Marca de tiempo de la primera muestra, en muestras de 48 kHz.
    pub pts: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AudioPacketError {
    #[error("datagrama de audio demasiado corto")]
    TooShort,
    #[error("no es un datagrama de audio")]
    WrongKind,
    #[error("carga de audio vacía o demasiado grande ({0} B)")]
    BadPayload(usize),
}

impl AudioHeader {
    /// Construye el datagrama completo (cabecera + carga Opus).
    pub fn encode(&self, payload: &[u8]) -> Result<Vec<u8>, AudioPacketError> {
        if payload.is_empty() || payload.len() > MAX_AUDIO_PAYLOAD {
            return Err(AudioPacketError::BadPayload(payload.len()));
        }
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + payload.len());
        out.push(AUDIO_KIND);
        out.push(0);
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.pts.to_be_bytes());
        out.extend_from_slice(payload);
        Ok(out)
    }

    /// Separa cabecera y carga. Rechaza lo que no sea audio o esté mal formado.
    pub fn decode(datagram: &[u8]) -> Result<(AudioHeader, &[u8]), AudioPacketError> {
        if datagram.len() < AUDIO_HEADER_LEN {
            return Err(AudioPacketError::TooShort);
        }
        if datagram[0] != AUDIO_KIND {
            return Err(AudioPacketError::WrongKind);
        }
        let payload = &datagram[AUDIO_HEADER_LEN..];
        if payload.is_empty() || payload.len() > MAX_AUDIO_PAYLOAD {
            return Err(AudioPacketError::BadPayload(payload.len()));
        }
        let u32_at = |i: usize| {
            u32::from_be_bytes([
                datagram[i],
                datagram[i + 1],
                datagram[i + 2],
                datagram[i + 3],
            ])
        };
        Ok((
            AudioHeader {
                stream_id: u16::from_be_bytes([datagram[2], datagram[3]]),
                seq: u32_at(4),
                pts: u32_at(8),
            },
            payload,
        ))
    }
}

/// ¿Es un datagrama de audio? (solo mira el primer byte).
pub fn is_audio(datagram: &[u8]) -> bool {
    datagram.first() == Some(&AUDIO_KIND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = AudioHeader {
            stream_id: 7,
            seq: 0xDEAD_BEEF,
            pts: 480_000,
        };
        let d = h.encode(&[1, 2, 3]).unwrap();
        assert_eq!(d.len(), AUDIO_HEADER_LEN + 3);
        assert!(is_audio(&d));
        let (h2, p) = AudioHeader::decode(&d).unwrap();
        assert_eq!(h2, h);
        assert_eq!(p, &[1, 2, 3]);
    }

    #[test]
    fn rejects_video_and_garbage() {
        // Un datagrama de vídeo empieza por la versión de vídeo (1).
        let mut video = vec![crate::video::WIRE_VERSION; 40];
        video[1] = 0;
        assert!(!is_audio(&video));
        assert_eq!(
            AudioHeader::decode(&video),
            Err(AudioPacketError::WrongKind)
        );
        assert_eq!(
            AudioHeader::decode(&[AUDIO_KIND; 5]),
            Err(AudioPacketError::TooShort)
        );
        let empty = AudioHeader {
            stream_id: 1,
            seq: 0,
            pts: 0,
        };
        assert!(empty.encode(&[]).is_err());
        assert!(empty.encode(&vec![0; MAX_AUDIO_PAYLOAD + 1]).is_err());
        let mut d = empty.encode(&[9]).unwrap();
        d.truncate(AUDIO_HEADER_LEN);
        assert!(matches!(
            AudioHeader::decode(&d),
            Err(AudioPacketError::BadPayload(0))
        ));
    }
}
