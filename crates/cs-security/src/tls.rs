//! Construcción de configuraciones rustls (solo TLS 1.3, autenticación mutua).

use crate::identity::Identity;
use crate::verifier::{PeerPolicy, PinnedVerifier};
use crate::SecurityError;
use rustls::crypto::CryptoProvider;
use std::sync::Arc;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn client_config(
    identity: &Identity,
    policy: PeerPolicy,
    alpn: &[u8],
) -> Result<rustls::ClientConfig, SecurityError> {
    let provider = provider();
    let verifier = PinnedVerifier::new(policy, &provider);
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert_der().clone()], identity.private_key())?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}

/// Un servidor con `PeerPolicy::AnyForPairing` debe ser un listener aparte,
/// activo solo mientras se muestra el PIN; nunca el listener de sesiones.
pub fn server_config(
    identity: &Identity,
    policy: PeerPolicy,
    alpn: &[u8],
) -> Result<rustls::ServerConfig, SecurityError> {
    let provider = provider();
    let verifier = PinnedVerifier::new(policy, &provider);
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert_der().clone()], identity.private_key())?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}
