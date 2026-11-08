//! Protocolo de cable de CrossScreen.
//!
//! - [`control`]: mensajes de control (stream QUIC fiable), serializados con
//!   postcard y enmarcados con prefijo de longitud ([`framing`]).
//! - [`video`]: cabecera binaria fija de los fragmentos de vídeo que viajan en
//!   datagramas QUIC.
//!
//! Ver `docs/04-protocolos-streaming.md`.

pub mod control;
pub mod framing;
pub mod video;

use serde::{Deserialize, Serialize};

/// Versión del protocolo. Un `major` distinto implica incompatibilidad; un
/// `minor` mayor solo añade mensajes o campos opcionales.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

impl ProtocolVersion {
    pub fn is_compatible_with(&self, other: &ProtocolVersion) -> bool {
        self.major == other.major
    }
}

/// ALPN usado en el handshake TLS de QUIC. Incluye la versión mayor para que
/// pares incompatibles fallen ya en el handshake con un error claro.
pub const ALPN: &[u8] = b"crossscreen/0";
/// ALPN de las conexiones de emparejamiento (sin certificados fijados todavía).
pub const ALPN_PAIRING: &[u8] = b"crossscreen-pair/0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility() {
        let a = ProtocolVersion { major: 1, minor: 0 };
        let b = ProtocolVersion { major: 1, minor: 7 };
        let c = ProtocolVersion { major: 2, minor: 0 };
        assert!(a.is_compatible_with(&b));
        assert!(!a.is_compatible_with(&c));
    }
}
