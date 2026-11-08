//! Transporte de CrossScreen. Ver `docs/adr/0002-transporte-quic.md`.
//!
//! - [`packetizer`] / [`reassembly`]: frame codificado ⇄ datagramas con FEC
//!   Reed-Solomon (puro, sin I/O, testeable de forma determinista).
//! - [`endpoint`]: endpoints QUIC (quinn) con TLS 1.3 mutuo y huellas fijadas.
//! - [`framed`]: canal de mensajes enmarcados sobre un stream QUIC.
//! - [`pairing`]: protocolo de emparejamiento sobre QUIC.
//! - [`pacer`]: reparto (*pacing*) de los datagramas de un frame con un
//!   presupuesto de bytes y espera de alta resolución.
//! - [`os`]: espera de alta resolución y tiempo de CPU del proceso.
//! - [`fec`]: FEC adaptativo (sans-IO) a partir de `ReceiverStats`.
//! - [`simloss`]: **solo pruebas**, inyector de pérdidas en ráfaga
//!   (Gilbert-Elliott) para reproducir una Wi-Fi en loopback.

pub mod endpoint;
pub mod fec;
pub mod framed;
pub mod os;
pub mod pacer;
pub mod packetizer;
pub mod pairing;
pub mod reassembly;
pub mod simloss;

pub use endpoint::{Connection, Endpoint};
pub use framed::FramedStream;
pub use pacer::{PacedSendOptions, Pacer, PacerConfig, PacingMode, WaitStrategy};
pub use packetizer::{Packetizer, PacketizerConfig};
pub use reassembly::{CompleteFrame, Reassembler, ReassemblyEvent, ReassemblyStats};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("seguridad: {0}")]
    Security(#[from] cs_security::SecurityError),
    #[error("configuración TLS/QUIC: {0}")]
    Crypto(String),
    #[error("conexión: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("conexión perdida: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("lectura de stream: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("escritura de stream: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("envío de datagrama: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("protocolo: {0}")]
    Framing(#[from] cs_protocol::framing::FramingError),
    #[error("emparejamiento: {0}")]
    Pairing(#[from] cs_security::pairing::PairingError),
    #[error("{0}")]
    RateLimited(#[from] cs_security::ratelimit::Locked),
    #[error("el par cerró el canal inesperadamente")]
    UnexpectedEof,
    #[error("mensaje inesperado: {0}")]
    UnexpectedMessage(&'static str),
    #[error("el par no presentó certificado")]
    NoPeerCertificate,
}
