//! Protocolo de cable de CrossScreen.
//!
//! - [`control`]: mensajes de control (stream QUIC fiable), serializados con
//!   postcard y enmarcados con prefijo de longitud ([`framing`]).
//! - [`video`]: cabecera binaria fija de los fragmentos de vídeo que viajan en
//!   datagramas QUIC.
//! - [`audio`]: datagramas de audio Opus (ADR-0008).
//!
//! Ver `docs/04-protocolos-streaming.md`.

pub mod audio;
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

/// Historial de versiones menores (compatibles entre sí; los mensajes que un
/// par no conoce se ignoran, ver `framing::FramingError::UnknownMessage`):
/// - 0.1: MVP (vídeo, control, cursor).
/// - 0.2: entrada remota (`ControlMessage::Input`, F6).
/// - 0.3: portapapeles de texto (`ControlMessage::ClipboardText`, F7).
/// - 0.4: audio (`AudioRequest`, `AudioStart`, `AudioStop` y datagramas de
///   audio, ADR-0008).
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 4 };

impl ProtocolVersion {
    /// Funciones que se pierden al hablar con un par de versión menor más
    /// antigua (para avisar al usuario en vez de fallar en silencio).
    pub fn missing_features_in(&self, peer: &ProtocolVersion) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.major == peer.major {
            if peer.minor < 2 && self.minor >= 2 {
                v.push("entrada remota (ratón y teclado)");
            }
            if peer.minor < 3 && self.minor >= 3 {
                v.push("portapapeles");
            }
            if peer.minor < 4 && self.minor >= 4 {
                v.push("audio");
            }
        }
        v
    }
}

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
