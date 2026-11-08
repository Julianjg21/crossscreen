//! Protocolo de emparejamiento sobre QUIC (ver `cs_security::pairing`).
//!
//! El *responder* abre un listener aparte con `PeerPolicy::AnyForPairing` y
//! ALPN `crossscreen-pair/0`, activo solo mientras muestra el PIN.

use crate::endpoint::{Connection, Endpoint};
use crate::framed::FramedStream;
use crate::TransportError;
use cs_protocol::ALPN_PAIRING;
use cs_security::pairing::{PairingRole, PairingSession, Pin};
use cs_security::ratelimit::AttemptLimiter;
use cs_security::verifier::PeerPolicy;
use cs_security::Fingerprint;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Instant;

#[derive(Debug, Serialize, Deserialize)]
enum PairingWire {
    Start { device_name: String, spake: Vec<u8> },
    Confirm { mac: [u8; 32] },
    Rejected,
}

/// Par emparejado con éxito; el llamante lo guarda en el `TrustStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub fingerprint: Fingerprint,
    pub name: String,
}

/// Lado que teclea el PIN.
pub async fn pair_as_initiator(
    endpoint: &Endpoint,
    addr: SocketAddr,
    pin: &Pin,
    my_name: &str,
) -> Result<PairedPeer, TransportError> {
    let conn = endpoint
        .connect(addr, PeerPolicy::AnyForPairing, ALPN_PAIRING)
        .await?;
    let my_fp = endpoint.identity().fingerprint();
    let peer_fp = conn.peer_fingerprint();
    let mut ch = FramedStream::new(conn.open_bi().await?);

    let (mut session, spake) = PairingSession::start(PairingRole::Initiator, pin, my_fp, peer_fp);
    ch.send(&PairingWire::Start {
        device_name: my_name.to_owned(),
        spake,
    })
    .await?;
    let (peer_name, peer_spake) = match ch.expect::<PairingWire>().await? {
        PairingWire::Start { device_name, spake } => (device_name, spake),
        PairingWire::Rejected => {
            return Err(TransportError::UnexpectedMessage(
                "el par rechazó el intento",
            ))
        }
        PairingWire::Confirm { .. } => {
            return Err(TransportError::UnexpectedMessage("se esperaba Start"))
        }
    };
    let my_mac = session.on_peer_spake(&peer_spake)?;
    ch.send(&PairingWire::Confirm { mac: my_mac }).await?;
    let result = match ch.expect::<PairingWire>().await? {
        PairingWire::Confirm { mac } => session.verify_peer_confirm(&mac),
        PairingWire::Rejected => Err(cs_security::pairing::PairingError::ConfirmationMismatch),
        PairingWire::Start { .. } => {
            return Err(TransportError::UnexpectedMessage("se esperaba Confirm"))
        }
    };
    ch.finish();
    conn.close(b"pairing done");
    result?;
    Ok(PairedPeer {
        fingerprint: peer_fp,
        name: sanitize_name(&peer_name),
    })
}

/// Lado que muestra el PIN: atiende intentos hasta un emparejamiento correcto
/// o hasta que el limitador bloquee. Cada intento fallido cuenta.
pub async fn pair_as_responder(
    endpoint: &Endpoint,
    pin: &Pin,
    my_name: &str,
    limiter: &mut AttemptLimiter,
) -> Result<PairedPeer, TransportError> {
    loop {
        let Some(conn) = endpoint.accept().await else {
            return Err(TransportError::UnexpectedEof);
        };
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("handshake de emparejamiento fallido: {e}");
                continue;
            }
        };
        if let Err(locked) = limiter.check(Instant::now()) {
            reject(&conn).await;
            return Err(locked.into());
        }
        match responder_attempt(endpoint, &conn, pin, my_name).await {
            Ok(peer) => {
                limiter.record_success();
                return Ok(peer);
            }
            Err(e) => {
                tracing::warn!(remote = %conn.remote_addr(), "intento de emparejamiento fallido: {e}");
                limiter.record_failure(Instant::now());
            }
        }
    }
}

async fn responder_attempt(
    endpoint: &Endpoint,
    conn: &Connection,
    pin: &Pin,
    my_name: &str,
) -> Result<PairedPeer, TransportError> {
    let my_fp = endpoint.identity().fingerprint();
    let peer_fp = conn.peer_fingerprint();
    let mut ch = FramedStream::new(conn.accept_bi().await?);
    let (peer_name, peer_spake) = match ch.expect::<PairingWire>().await? {
        PairingWire::Start { device_name, spake } => (device_name, spake),
        _ => return Err(TransportError::UnexpectedMessage("se esperaba Start")),
    };
    let (mut session, spake) = PairingSession::start(PairingRole::Responder, pin, peer_fp, my_fp);
    ch.send(&PairingWire::Start {
        device_name: my_name.to_owned(),
        spake,
    })
    .await?;
    let my_mac = session.on_peer_spake(&peer_spake)?;
    let peer_mac = match ch.expect::<PairingWire>().await? {
        PairingWire::Confirm { mac } => mac,
        _ => return Err(TransportError::UnexpectedMessage("se esperaba Confirm")),
    };
    // Verificar antes de enviar nuestra confirmación.
    if let Err(e) = session.verify_peer_confirm(&peer_mac) {
        let _ = ch.send(&PairingWire::Rejected).await;
        ch.finish();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn.closed()).await;
        return Err(e.into());
    }
    ch.send(&PairingWire::Confirm { mac: my_mac }).await?;
    ch.finish();
    // Esperar a que el iniciador cierre para no cortar el último mensaje.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn.closed()).await;
    Ok(PairedPeer {
        fingerprint: peer_fp,
        name: sanitize_name(&peer_name),
    })
}

async fn reject(conn: &Connection) {
    if let Ok(pair) = conn.accept_bi().await {
        let mut ch = FramedStream::new(pair);
        let _ = ch.recv::<PairingWire>().await;
        let _ = ch.send(&PairingWire::Rejected).await;
        ch.finish();
        // Cerrar de inmediato descartaría el `Rejected` aún no entregado.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn.closed()).await;
    }
    conn.close(b"rate limited");
}

/// Nombres de dispositivo: imprimibles y acotados (vienen de un par aún no confiable).
fn sanitize_name(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    if cleaned.trim().is_empty() {
        "Dispositivo sin nombre".to_owned()
    } else {
        cleaned
    }
}
