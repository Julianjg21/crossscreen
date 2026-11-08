//! Seguridad de CrossScreen. Ver `docs/adr/0004-seguridad-emparejamiento.md`.
//!
//! - [`identity`]: par de claves Ed25519 + certificado autofirmado por dispositivo.
//! - [`pairing`]: emparejamiento con PIN de 6 dígitos mediante SPAKE2, que
//!   autentica las huellas de certificado vistas en el handshake TLS.
//! - [`verifier`]: verificadores rustls que solo aceptan huellas fijadas.
//! - [`trust`]: almacén persistente de pares emparejados.
//! - [`ratelimit`]: limitador de intentos fallidos con bloqueo exponencial.

pub mod identity;
pub mod pairing;
pub mod ratelimit;
pub mod tls;
pub mod trust;
pub mod verifier;

#[cfg(windows)]
mod dpapi;
mod fingerprint;
pub use fingerprint::Fingerprint;

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("generación de certificado: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("almacén de confianza corrupto: {0}")]
    TrustStore(#[from] serde_json::Error),
    #[error("identidad inválida: {0}")]
    InvalidIdentity(&'static str),
}
