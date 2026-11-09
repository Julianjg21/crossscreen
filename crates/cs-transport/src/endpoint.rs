//! Endpoints QUIC con TLS 1.3 mutuo y huellas fijadas.

use crate::TransportError;
use bytes::Bytes;
use cs_security::identity::{Identity, CERT_SERVER_NAME};
use cs_security::verifier::PeerPolicy;
use cs_security::{tls, Fingerprint};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::CertificateDer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Parámetros de transporte comunes a cliente y servidor.
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(
        Duration::from_secs(10)
            .try_into()
            .expect("idle timeout válido"),
    ));
    // Buffers holgados para ráfagas de un IDR (~1-2 MB).
    t.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    t.datagram_send_buffer_size(8 * 1024 * 1024);
    Arc::new(t)
}

/// Buffers del socket UDP. Los valores por defecto del SO (64 KB en Windows)
/// se desbordan con la ráfaga de un solo frame grande y provocan pérdidas
/// incluso en loopback (detectado por `tests/loopback.rs`).
const SOCKET_BUFFER_BYTES: usize = 8 * 1024 * 1024;

fn make_endpoint(
    bind: SocketAddr,
    server: Option<quinn::ServerConfig>,
) -> Result<quinn::Endpoint, TransportError> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    if bind.is_ipv6() {
        // Doble pila cuando se liga a [::].
        let _ = socket.set_only_v6(false);
    }
    for (what, res) in [
        (
            "SO_RCVBUF",
            socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES),
        ),
        (
            "SO_SNDBUF",
            socket.set_send_buffer_size(SOCKET_BUFFER_BYTES),
        ),
    ] {
        if let Err(e) = res {
            tracing::warn!("no se pudo ajustar {what}: {e}");
        }
    }
    if let Ok(n) = socket.recv_buffer_size() {
        if n < SOCKET_BUFFER_BYTES {
            // Linux limita a net.core.rmem_max; avisar para diagnóstico.
            tracing::info!(
                got = n,
                wanted = SOCKET_BUFFER_BYTES,
                "buffer de recepción UDP limitado por el SO"
            );
        }
    }
    socket.bind(&bind.into())?;
    let runtime = quinn::default_runtime()
        .ok_or_else(|| TransportError::Crypto("no hay runtime async compatible".into()))?;
    Ok(quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        server,
        socket.into(),
        runtime,
    )?)
}

fn crypto_err(e: impl std::fmt::Display) -> TransportError {
    TransportError::Crypto(e.to_string())
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    inner: quinn::Endpoint,
    identity: Identity,
}

impl Endpoint {
    /// Servidor que acepta conexiones de pares según `policy`.
    pub fn server(
        bind: SocketAddr,
        identity: Identity,
        policy: PeerPolicy,
        alpn: &[u8],
    ) -> Result<Self, TransportError> {
        let rustls_cfg = tls::server_config(&identity, policy, alpn)?;
        let quic = QuicServerConfig::try_from(rustls_cfg).map_err(crypto_err)?;
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
        cfg.transport_config(transport_config());
        let inner = make_endpoint(bind, Some(cfg))?;
        Ok(Endpoint { inner, identity })
    }

    /// Endpoint solo cliente, ligado a un puerto efímero.
    pub fn client(bind: SocketAddr, identity: Identity) -> Result<Self, TransportError> {
        let inner = make_endpoint(bind, None)?;
        Ok(Endpoint { inner, identity })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.inner.local_addr()?)
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        policy: PeerPolicy,
        alpn: &[u8],
    ) -> Result<Connection, TransportError> {
        let rustls_cfg = tls::client_config(&self.identity, policy, alpn)?;
        let quic = QuicClientConfig::try_from(rustls_cfg).map_err(crypto_err)?;
        let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
        cfg.transport_config(transport_config());
        let conn = self
            .inner
            .connect_with(cfg, addr, CERT_SERVER_NAME)?
            .await?;
        Connection::new(conn)
    }

    /// Espera la siguiente conexión entrante y completa el handshake.
    /// Devuelve `None` si el endpoint se cerró.
    pub async fn accept(&self) -> Option<Result<Connection, TransportError>> {
        let incoming = self.inner.accept().await?;
        Some(match incoming.await {
            Ok(c) => Connection::new(c),
            Err(e) => Err(e.into()),
        })
    }

    pub fn close(&self) {
        self.inner.close(0u32.into(), b"bye");
    }
}

/// Conexión autenticada con un par.
#[derive(Debug, Clone)]
pub struct Connection {
    inner: quinn::Connection,
    peer: Fingerprint,
}

impl Connection {
    fn new(inner: quinn::Connection) -> Result<Self, TransportError> {
        let certs = inner
            .peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
            .ok_or(TransportError::NoPeerCertificate)?;
        let leaf = certs.first().ok_or(TransportError::NoPeerCertificate)?;
        let peer = Fingerprint::of_cert_der(leaf);
        Ok(Connection { inner, peer })
    }

    /// Huella del certificado que el par presentó en el handshake.
    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.inner.remote_address()
    }

    pub fn rtt(&self) -> Duration {
        self.inner.rtt()
    }

    /// Tamaño máximo de datagrama en la ruta actual (`None` si el par no
    /// admite datagramas).
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.inner.max_datagram_size()
    }

    pub fn send_datagram(&self, data: Bytes) -> Result<(), TransportError> {
        Ok(self.inner.send_datagram(data)?)
    }

    pub async fn read_datagram(&self) -> Result<Bytes, TransportError> {
        Ok(self.inner.read_datagram().await?)
    }

    pub async fn open_bi(&self) -> Result<(quinn::SendStream, quinn::RecvStream), TransportError> {
        Ok(self.inner.open_bi().await?)
    }

    pub async fn accept_bi(
        &self,
    ) -> Result<(quinn::SendStream, quinn::RecvStream), TransportError> {
        Ok(self.inner.accept_bi().await?)
    }

    /// Stream unidireccional saliente (transferencia de archivos).
    pub async fn open_uni(&self) -> Result<quinn::SendStream, TransportError> {
        Ok(self.inner.open_uni().await?)
    }

    /// Siguiente stream unidireccional que abra el par.
    pub async fn accept_uni(&self) -> Result<quinn::RecvStream, TransportError> {
        Ok(self.inner.accept_uni().await?)
    }

    pub fn stats(&self) -> quinn::ConnectionStats {
        self.inner.stats()
    }

    pub fn close(&self, reason: &[u8]) {
        self.inner.close(0u32.into(), reason);
    }

    pub async fn closed(&self) -> quinn::ConnectionError {
        self.inner.closed().await
    }
}
