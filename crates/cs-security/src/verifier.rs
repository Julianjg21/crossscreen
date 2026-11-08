//! Verificadores rustls que autentican al par por huella de certificado
//! fijada, en lugar de por cadena de CA.
//!
//! La firma del handshake **siempre** se verifica contra la clave pública del
//! certificado presentado (prueba de posesión de la clave privada). Lo que
//! cambia según [`PeerPolicy`] es qué certificados se aceptan.

use crate::Fingerprint;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    verify_tls12_signature, verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub enum PeerPolicy {
    /// Acepta cualquier huella del conjunto (servidor con varios pares
    /// emparejados). El conjunto es compartido y puede actualizarse en caliente.
    Pinned(Arc<RwLock<HashSet<Fingerprint>>>),
    /// Acepta exactamente esta huella (cliente conectando a un par concreto).
    Exactly(Fingerprint),
    /// Solo para conexiones de emparejamiento (ALPN distinto): acepta
    /// cualquier certificado; la autenticación la aporta SPAKE2 sobre las
    /// huellas observadas. Nunca debe usarse para sesiones normales.
    AnyForPairing,
}

impl PeerPolicy {
    pub fn pinned(fps: impl IntoIterator<Item = Fingerprint>) -> Self {
        PeerPolicy::Pinned(Arc::new(RwLock::new(fps.into_iter().collect())))
    }

    fn allows(&self, fp: &Fingerprint) -> bool {
        match self {
            PeerPolicy::Pinned(set) => set.read().map(|s| s.contains(fp)).unwrap_or(false),
            PeerPolicy::Exactly(expected) => expected == fp,
            PeerPolicy::AnyForPairing => true,
        }
    }
}

#[derive(Debug)]
pub struct PinnedVerifier {
    policy: PeerPolicy,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    pub fn new(policy: PeerPolicy, provider: &CryptoProvider) -> Arc<Self> {
        Arc::new(PinnedVerifier {
            policy,
            algs: provider.signature_verification_algorithms,
        })
    }

    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), Error> {
        // Los dispositivos presentan un único certificado autofirmado.
        if !intermediates.is_empty() {
            return Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        let fp = Fingerprint::of_cert_der(end_entity);
        if self.policy.allows(&fp) {
            Ok(())
        } else {
            tracing::warn!(peer = %fp.short(), "certificado no fijado rechazado");
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check(end_entity, intermediates)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check(end_entity, intermediates)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
