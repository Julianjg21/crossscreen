//! Enmarcado de mensajes de control: `u32` little-endian con la longitud,
//! seguido del mensaje serializado con postcard.

use serde::{de::DeserializeOwned, Serialize};

/// Límite duro por mensaje de control. Los mensajes legítimos son de pocos KB;
/// el límite evita reservas de memoria arbitrarias por un par malicioso.
pub const MAX_FRAME_LEN: usize = 256 * 1024;
const LEN_PREFIX: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("mensaje demasiado grande: {0} bytes (máx. {MAX_FRAME_LEN})")]
    TooLarge(usize),
    #[error("error de serialización: {0}")]
    Codec(#[from] postcard::Error),
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FramingError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(FramingError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(LEN_PREFIX + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decodificador incremental: se le añaden bytes según llegan del stream y
/// devuelve mensajes completos.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Devuelve el siguiente mensaje completo, `Ok(None)` si faltan bytes.
    /// Tras un error el stream debe cerrarse: el decodificador queda desincronizado.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FramingError> {
        if self.buf.len() < LEN_PREFIX {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..LEN_PREFIX].try_into().expect("4 bytes")) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FramingError::TooLarge(len));
        }
        if self.buf.len() < LEN_PREFIX + len {
            return Ok(None);
        }
        let msg = postcard::from_bytes(&self.buf[LEN_PREFIX..LEN_PREFIX + len])?;
        self.buf.drain(..LEN_PREFIX + len);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::*;
    use crate::PROTOCOL_VERSION;
    use cs_core::{DeviceId, Roles};
    use proptest::prelude::*;

    fn hello(name: String) -> ControlMessage {
        ControlMessage::Hello(Hello {
            version: PROTOCOL_VERSION,
            device_id: DeviceId([7; 16]),
            device_name: name,
            roles: Roles::BOTH,
        })
    }

    #[test]
    fn roundtrip_multiple_messages() {
        let msgs = vec![
            hello("pc-oficina".into()),
            ControlMessage::RequestKeyframe { stream_id: 3 },
            ControlMessage::Ping {
                nonce: 42,
                sent_us: 1_000,
            },
            ControlMessage::Bye {
                reason: ByeReason::UserRequested,
            },
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            wire.extend(encode(m).unwrap());
        }
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        let mut out = Vec::new();
        while let Some(m) = dec.next_frame::<ControlMessage>().unwrap() {
            out.push(m);
        }
        assert_eq!(out, msgs);
    }

    #[test]
    fn rejects_oversized_length_prefix() {
        let mut dec = FrameDecoder::new();
        dec.push(&(u32::MAX).to_le_bytes());
        assert!(matches!(
            dec.next_frame::<ControlMessage>(),
            Err(FramingError::TooLarge(_))
        ));
    }

    proptest! {
        /// Partir el flujo de bytes en trozos arbitrarios no cambia el resultado.
        #[test]
        fn arbitrary_chunking(name in "[a-z]{0,40}", cuts in proptest::collection::vec(1usize..16, 0..20)) {
            let m1 = hello(name);
            let m2 = ControlMessage::StopStream { stream_id: 9 };
            let mut wire = encode(&m1).unwrap();
            wire.extend(encode(&m2).unwrap());
            let mut dec = FrameDecoder::new();
            let mut out = Vec::new();
            let mut pos = 0;
            for c in cuts.into_iter().chain(std::iter::repeat(usize::MAX)) {
                if pos >= wire.len() { break; }
                let end = pos.saturating_add(c).min(wire.len());
                dec.push(&wire[pos..end]);
                pos = end;
                while let Some(m) = dec.next_frame::<ControlMessage>().unwrap() { out.push(m); }
            }
            prop_assert_eq!(out, vec![m1, m2]);
        }

        /// Basura aleatoria nunca provoca pánico.
        #[test]
        fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut dec = FrameDecoder::new();
            dec.push(&bytes);
            let _ = dec.next_frame::<ControlMessage>();
        }
    }
}
