//! Reconexión del emisor (Windows y Linux): si se corta la red, se vuelve a
//! conectar con el mismo receptor durante una ventana de gracia, con
//! *backoff*, conservando la captura (y el monitor virtual) y repitiendo el
//! *handshake* con el mismo modo.

use crate::SenderConfig;
use anyhow::{bail, Context, Result};
use cs_core::{Mode, Roles, StreamMode};
use cs_protocol::control::{ByeReason, ControlMessage, Hello, SinkCapabilities, StartStream};
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::verifier::PeerPolicy;
use cs_transport::{Connection, FramedStream};
use std::time::Duration;
use tokio::sync::watch;

use crate::STREAM_ID;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(15);

async fn recv_msg(ctl: &mut FramedStream, what: &'static str) -> Result<ControlMessage> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, ctl.expect::<ControlMessage>())
        .await
        .with_context(|| format!("el receptor no envió {what} a tiempo"))?
        .with_context(|| format!("esperando {what}"))
}

async fn sleep_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Negocia el canal de control de una reconexión manteniendo el `display_mode`
/// ya activo en el monitor virtual y el hilo de captura.
async fn handshake_reconnect(
    conn: &Connection,
    cfg: &SenderConfig,
    display_mode: Mode,
    stream_mode: StreamMode,
) -> Result<FramedStream> {
    let mut ctl = FramedStream::new(conn.open_bi().await.context("abriendo canal de control")?);
    ctl.send(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        device_id: cfg.device_id,
        device_name: cfg.device_name.clone(),
        roles: Roles {
            source: true,
            sink: false,
        },
    }))
    .await?;
    let peer = match recv_msg(&mut ctl, "Hello").await? {
        ControlMessage::Hello(h) => h,
        ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
        other => bail!("se esperaba Hello y llegó {other:?}"),
    };
    if !PROTOCOL_VERSION.is_compatible_with(&peer.version) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::IncompatibleVersion,
            })
            .await;
        ctl.finish();
        bail!(
            "versión de protocolo incompatible: local {:?}, receptor {:?}",
            PROTOCOL_VERSION,
            peer.version
        );
    }
    let sink: SinkCapabilities = loop {
        match recv_msg(&mut ctl, "SinkCapabilities").await? {
            ControlMessage::SinkCapabilities(c) => break c,
            ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
            other => tracing::debug!(?other, "mensaje ignorado antes de SinkCapabilities"),
        }
    };
    let codec = cfg.negotiated_codec;
    if !sink.decoders.iter().any(|d| d.codec == codec) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::Error,
            })
            .await;
        bail!("el receptor ya no anuncia decodificador {codec:?}");
    }
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: STREAM_ID,
        mode: stream_mode,
        codec,
        display_mode,
        bitrate_kbps: cfg.bitrate_kbps,
    }))
    .await?;
    let started = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            match ctl.expect::<ControlMessage>().await? {
                ControlMessage::StreamStarted { stream_id } if stream_id == STREAM_ID => {
                    return Ok(())
                }
                ControlMessage::StreamFailed { reason, .. } => {
                    bail!("el receptor no pudo iniciar el flujo: {reason}")
                }
                ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
                other => tracing::debug!(?other, "mensaje ignorado antes de StreamStarted"),
            }
        }
    })
    .await;
    match started {
        Ok(Ok(())) => Ok(ctl),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("el receptor no confirmó StreamStarted en {START_TIMEOUT:?}"),
    }
}

/// Intenta reconectar al receptor durante `reconn.grace` con *backoff*
/// exponencial (250 ms → 2 s). Devuelve `Ok(None)` si el usuario pide parar o
/// se cumple `deadline` durante la espera.
pub(crate) async fn try_reconnect(
    reconn: &crate::ReconnectConfig,
    cfg: &SenderConfig,
    display_mode: Mode,
    stream_mode: StreamMode,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<(Connection, FramedStream)>> {
    let grace_deadline = tokio::time::Instant::now() + reconn.grace;
    let mut delay = Duration::from_millis(250);
    let mut last_err = anyhow::anyhow!("tiempo de gracia agotado");

    loop {
        let now = tokio::time::Instant::now();
        if now >= grace_deadline {
            return Err(last_err);
        }
        let rem = grace_deadline - now;
        let attempt_timeout = rem.min(Duration::from_secs(4));
        let attempt = async {
            let conn = reconn
                .endpoint
                .connect(reconn.addr, PeerPolicy::Exactly(reconn.peer), ALPN)
                .await
                .context("conectando QUIC")?;
            match handshake_reconnect(&conn, cfg, display_mode, stream_mode).await {
                Ok(ctl) => Ok((conn, ctl)),
                Err(e) => {
                    conn.close(b"handshake failed");
                    Err(e)
                }
            }
        };

        tokio::select! {
            r = shutdown.changed() => {
                if r.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
            _ = sleep_opt(deadline) => return Ok(None),
            res = tokio::time::timeout(attempt_timeout, attempt) => {
                match res {
                    Ok(Ok(pair)) => return Ok(Some(pair)),
                    Ok(Err(e)) => last_err = e,
                    Err(_) => last_err = anyhow::anyhow!("tiempo agotado en intento de reconexión"),
                }
            }
        }

        let now = tokio::time::Instant::now();
        if now >= grace_deadline {
            return Err(last_err);
        }
        let wait = delay.min(grace_deadline - now);
        tokio::select! {
            r = shutdown.changed() => {
                if r.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
            _ = sleep_opt(deadline) => return Ok(None),
            _ = tokio::time::sleep(wait) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReconnectConfig;
    use cs_core::Codec;
    use cs_core::CodecSupport;
    use cs_security::identity::Identity;
    use cs_transport::Endpoint;

    #[tokio::test]
    async fn try_reconnect_recovers_within_grace_window() {
        let src_id = Identity::generate().unwrap();
        let sink_id = Identity::generate().unwrap();
        let src_fp = src_id.fingerprint();
        let sink_fp = sink_id.fingerprint();

        let server = Endpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            sink_id,
            PeerPolicy::Exactly(src_fp),
            ALPN,
        )
        .unwrap();
        let addr = server.local_addr().unwrap();

        let srv = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().unwrap();
            let mut ctl = FramedStream::new(conn.accept_bi().await.unwrap());
            let _hello: ControlMessage = ctl.expect().await.unwrap();
            ctl.send(&ControlMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                device_id: sink_fp.device_id(),
                device_name: "sink-test".into(),
                roles: Roles {
                    source: false,
                    sink: true,
                },
            }))
            .await
            .unwrap();
            ctl.send(&ControlMessage::SinkCapabilities(SinkCapabilities {
                native_mode: Mode::new(1366, 768, 60),
                dpi: 96,
                decoders: vec![CodecSupport {
                    codec: Codec::H264,
                    max_h264_profile: None,
                    hardware: true,
                    max_pixel_rate: 0,
                }],
            }))
            .await
            .unwrap();
            let msg: ControlMessage = ctl.expect().await.unwrap();
            match msg {
                ControlMessage::StartStream(s) => {
                    assert_eq!(s.display_mode, Mode::new(1366, 768, 60));
                    ctl.send(&ControlMessage::StreamStarted {
                        stream_id: s.stream_id,
                    })
                    .await
                    .unwrap();
                }
                other => panic!("esperaba StartStream, llegó {other:?}"),
            }
            conn
        });

        let client = Endpoint::client("127.0.0.1:0".parse().unwrap(), src_id).unwrap();
        let reconn = ReconnectConfig {
            endpoint: client.clone(),
            addr,
            peer: sink_fp,
            grace: Duration::from_secs(5),
        };
        let cfg = SenderConfig::new(src_fp.device_id(), "src-test".into());
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        let res = try_reconnect(
            &reconn,
            &cfg,
            Mode::new(1366, 768, 60),
            StreamMode::Extend,
            &mut stop_rx,
            None,
        )
        .await
        .unwrap();
        assert!(res.is_some());
        let (conn, _ctl) = res.unwrap();
        let srv_conn = srv.await.unwrap();
        conn.close(b"done");
        srv_conn.close(b"done");
        client.close();
    }

    #[tokio::test]
    async fn try_reconnect_aborts_on_shutdown() {
        let src_id = Identity::generate().unwrap();
        let sink_id = Identity::generate().unwrap();
        let src_fp = src_id.fingerprint();
        let client = Endpoint::client("127.0.0.1:0".parse().unwrap(), src_id).unwrap();
        let reconn = ReconnectConfig {
            endpoint: client.clone(),
            // Puerto cerrado local para que falle o espere y cancelamos por shutdown.
            addr: "127.0.0.1:9".parse().unwrap(),
            peer: sink_id.fingerprint(),
            grace: Duration::from_secs(10),
        };
        let cfg = SenderConfig::new(src_fp.device_id(), "src-test".into());
        let (stop_tx, mut stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = stop_tx.send(true);
        });
        let res = try_reconnect(
            &reconn,
            &cfg,
            Mode::new(1366, 768, 60),
            StreamMode::Extend,
            &mut stop_rx,
            None,
        )
        .await
        .unwrap();
        assert!(res.is_none());
        client.close();
    }
}
