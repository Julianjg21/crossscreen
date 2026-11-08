//! Emparejamiento con PIN mediante SPAKE2 (implementación *sans-IO*).
//!
//! Flujo:
//! 1. El receptor (*responder*) muestra un [`Pin`] de 6 dígitos.
//! 2. La fuente (*initiator*) abre una conexión QUIC de emparejamiento en la
//!    que ninguno de los dos fija certificado todavía, pero ambos registran la
//!    huella del certificado que el otro presentó en el handshake TLS.
//! 3. Ambos ejecutan SPAKE2 simétrico con el PIN y derivan una clave común.
//! 4. Cada lado envía `HMAC(clave, etiqueta_rol || fp_initiator || fp_responder)`
//!    calculado con las huellas **que él vio**. Si hay un intermediario, las
//!    huellas vistas difieren y la confirmación falla.
//!
//! Un atacante pasivo no puede probar PINs offline (propiedad de SPAKE2) y uno
//! activo solo tiene un intento por sesión, limitado además por
//! [`crate::ratelimit::AttemptLimiter`].

use crate::Fingerprint;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use std::fmt;
use subtle::ConstantTimeEq;

const SPAKE_IDENTITY: &[u8] = b"crossscreen-pairing-v1";
const LABEL_INITIATOR: &[u8] = b"cs-pair-confirm-v1/initiator";
const LABEL_RESPONDER: &[u8] = b"cs-pair-confirm-v1/responder";

/// PIN de emparejamiento de 6 dígitos decimales.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin(String);

impl Pin {
    pub const LEN: usize = 6;

    pub fn generate() -> Self {
        let n: u32 = rand::rngs::OsRng.gen_range(0..1_000_000);
        Pin(format!("{n:06}"))
    }

    pub fn parse(s: &str) -> Result<Self, PairingError> {
        let s: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        if s.len() == Self::LEN && s.bytes().all(|b| b.is_ascii_digit()) {
            Ok(Pin(s))
        } else {
            Err(PairingError::InvalidPin)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(******)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingRole {
    /// Quien teclea el PIN (cliente QUIC).
    Initiator,
    /// Quien muestra el PIN (servidor QUIC).
    Responder,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairingError {
    #[error("el PIN debe tener exactamente 6 dígitos")]
    InvalidPin,
    #[error("mensaje SPAKE2 inválido")]
    BadSpakeMessage,
    #[error("la confirmación no coincide: PIN incorrecto o conexión interceptada")]
    ConfirmationMismatch,
    #[error("orden de mensajes incorrecto")]
    OutOfOrder,
}

type HmacSha256 = Hmac<Sha256>;

pub struct PairingSession {
    role: PairingRole,
    spake: Option<Spake2<Ed25519Group>>,
    key: Option<Vec<u8>>,
    initiator_fp: Fingerprint,
    responder_fp: Fingerprint,
}

impl fmt::Debug for PairingSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingSession")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl PairingSession {
    /// Inicia la sesión. `initiator_fp`/`responder_fp` son las huellas tal como
    /// las ve este lado (la propia y la observada en el handshake TLS).
    /// Devuelve el mensaje SPAKE2 a enviar al par.
    pub fn start(
        role: PairingRole,
        pin: &Pin,
        initiator_fp: Fingerprint,
        responder_fp: Fingerprint,
    ) -> (Self, Vec<u8>) {
        let (spake, msg) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(pin.as_str().as_bytes()),
            &SpakeIdentity::new(SPAKE_IDENTITY),
        );
        (
            PairingSession {
                role,
                spake: Some(spake),
                key: None,
                initiator_fp,
                responder_fp,
            },
            msg,
        )
    }

    /// Procesa el mensaje SPAKE2 del par y devuelve la confirmación propia.
    pub fn on_peer_spake(&mut self, msg: &[u8]) -> Result<[u8; 32], PairingError> {
        let spake = self.spake.take().ok_or(PairingError::OutOfOrder)?;
        let key = spake
            .finish(msg)
            .map_err(|_| PairingError::BadSpakeMessage)?;
        self.key = Some(key);
        Ok(self.mac(self.own_label()))
    }

    /// Verifica la confirmación del par. Si tiene éxito, el emparejamiento es
    /// válido y la huella del par puede guardarse en el almacén de confianza.
    pub fn verify_peer_confirm(&self, mac: &[u8]) -> Result<(), PairingError> {
        if self.key.is_none() {
            return Err(PairingError::OutOfOrder);
        }
        let expected = self.mac(self.peer_label());
        if expected.ct_eq(mac).into() {
            Ok(())
        } else {
            Err(PairingError::ConfirmationMismatch)
        }
    }

    fn own_label(&self) -> &'static [u8] {
        match self.role {
            PairingRole::Initiator => LABEL_INITIATOR,
            PairingRole::Responder => LABEL_RESPONDER,
        }
    }

    fn peer_label(&self) -> &'static [u8] {
        match self.role {
            PairingRole::Initiator => LABEL_RESPONDER,
            PairingRole::Responder => LABEL_INITIATOR,
        }
    }

    fn mac(&self, label: &[u8]) -> [u8; 32] {
        let key = self.key.as_deref().expect("clave derivada");
        let mut m =
            <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC acepta cualquier longitud");
        m.update(label);
        m.update(&self.initiator_fp.0);
        m.update(&self.responder_fp.0);
        m.finalize().into_bytes().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FI: Fingerprint = Fingerprint([1; 32]);
    const FR: Fingerprint = Fingerprint([2; 32]);
    const FMITM: Fingerprint = Fingerprint([9; 32]);

    /// Ejecuta el protocolo completo; devuelve el resultado de cada lado.
    fn run(
        pin_i: &Pin,
        pin_r: &Pin,
        view_i: (Fingerprint, Fingerprint),
        view_r: (Fingerprint, Fingerprint),
    ) -> (Result<(), PairingError>, Result<(), PairingError>) {
        let (mut i, msg_i) =
            PairingSession::start(PairingRole::Initiator, pin_i, view_i.0, view_i.1);
        let (mut r, msg_r) =
            PairingSession::start(PairingRole::Responder, pin_r, view_r.0, view_r.1);
        let conf_i = i.on_peer_spake(&msg_r).unwrap();
        let conf_r = r.on_peer_spake(&msg_i).unwrap();
        (
            i.verify_peer_confirm(&conf_r),
            r.verify_peer_confirm(&conf_i),
        )
    }

    #[test]
    fn same_pin_succeeds() {
        let pin = Pin::parse("123456").unwrap();
        let (a, b) = run(&pin, &pin, (FI, FR), (FI, FR));
        assert_eq!(a, Ok(()));
        assert_eq!(b, Ok(()));
    }

    #[test]
    fn wrong_pin_fails_both_sides() {
        let (a, b) = run(
            &Pin::parse("123456").unwrap(),
            &Pin::parse("123457").unwrap(),
            (FI, FR),
            (FI, FR),
        );
        assert_eq!(a, Err(PairingError::ConfirmationMismatch));
        assert_eq!(b, Err(PairingError::ConfirmationMismatch));
    }

    #[test]
    fn mitm_with_correct_pin_is_detected() {
        // El intermediario conoce el PIN pero presenta su propio certificado a
        // cada lado: las huellas vistas no coinciden.
        let pin = Pin::parse("000042").unwrap();
        let (a, b) = run(&pin, &pin, (FI, FMITM), (FMITM, FR));
        assert!(a.is_err());
        assert!(b.is_err());
    }

    #[test]
    fn pin_parsing() {
        assert!(Pin::parse("123 456").is_ok());
        assert!(Pin::parse("123-456").is_ok());
        assert_eq!(Pin::parse("12345"), Err(PairingError::InvalidPin));
        assert_eq!(Pin::parse("12345a"), Err(PairingError::InvalidPin));
        let p = Pin::generate();
        assert_eq!(p.as_str().len(), 6);
        assert_eq!(format!("{p:?}"), "Pin(******)");
    }

    #[test]
    fn rejects_out_of_order() {
        let pin = Pin::generate();
        let (s, _) = PairingSession::start(PairingRole::Initiator, &pin, FI, FR);
        assert_eq!(
            s.verify_peer_confirm(&[0; 32]),
            Err(PairingError::OutOfOrder)
        );
    }
}
