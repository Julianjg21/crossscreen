//! Pruebas de integración sobre QUIC real en loopback.

use bytes::Bytes;
use cs_protocol::control::ControlMessage;
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::pairing::Pin;
use cs_security::ratelimit::AttemptLimiter;
use cs_security::verifier::PeerPolicy;
use cs_transport::packetizer::FrameMeta;
use cs_transport::pairing::{pair_as_initiator, pair_as_responder};
use cs_transport::{
    Endpoint, FramedStream, Packetizer, PacketizerConfig, Reassembler, ReassemblyEvent,
    TransportError,
};
use std::net::SocketAddr;
use std::time::Duration;

fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

#[tokio::test]
async fn pairing_with_correct_pin_authenticates_both_fingerprints() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (fa, fb) = (a.fingerprint(), b.fingerprint());
    let pin = Pin::generate();

    let server = Endpoint::server(
        localhost(),
        b,
        PeerPolicy::AnyForPairing,
        cs_protocol::ALPN_PAIRING,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let pin_r = pin.clone();
    let responder = tokio::spawn(async move {
        let mut lim = AttemptLimiter::default();
        pair_as_responder(&server, &pin_r, "receptor", &mut lim).await
    });

    let client = Endpoint::client(localhost(), a).unwrap();
    let got_by_initiator = pair_as_initiator(&client, addr, &pin, "fuente")
        .await
        .unwrap();
    let got_by_responder = responder.await.unwrap().unwrap();

    assert_eq!(got_by_initiator.fingerprint, fb);
    assert_eq!(got_by_initiator.name, "receptor");
    assert_eq!(got_by_responder.fingerprint, fa);
    assert_eq!(got_by_responder.name, "fuente");
}

#[tokio::test]
async fn pairing_with_wrong_pin_fails_and_counts_attempt() {
    let server = Endpoint::server(
        localhost(),
        Identity::generate().unwrap(),
        PeerPolicy::AnyForPairing,
        cs_protocol::ALPN_PAIRING,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        // Bloquea tras 1 fallo para que la tarea termine.
        let mut lim = AttemptLimiter::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let pin = Pin::parse("111111").unwrap();
        pair_as_responder(&server, &pin, "receptor", &mut lim).await
    });

    let client = Endpoint::client(localhost(), Identity::generate().unwrap()).unwrap();
    let err = pair_as_initiator(&client, addr, &Pin::parse("222222").unwrap(), "atacante")
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::Pairing(_)), "{err:?}");

    // Segundo intento: el responder ya está bloqueado y lo rechaza.
    let err2 = pair_as_initiator(&client, addr, &Pin::parse("111111").unwrap(), "atacante")
        .await
        .unwrap_err();
    assert!(
        matches!(err2, TransportError::UnexpectedMessage(_)),
        "{err2:?}"
    );
    assert!(matches!(
        responder.await.unwrap(),
        Err(TransportError::RateLimited(_))
    ));
}

#[tokio::test]
async fn pinned_session_connects_and_exchanges_control_messages() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (fa, fb) = (a.fingerprint(), b.fingerprint());
    let server = Endpoint::server(localhost(), b, PeerPolicy::pinned([fa]), ALPN).unwrap();
    let addr = server.local_addr().unwrap();

    let srv = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().unwrap();
        assert_eq!(conn.peer_fingerprint(), fa);
        let mut ch = FramedStream::new(conn.accept_bi().await.unwrap());
        let msg: ControlMessage = ch.expect().await.unwrap();
        ch.send(&ControlMessage::Pong {
            nonce: 7,
            ping_sent_us: 1,
            received_us: 2,
        })
        .await
        .unwrap();
        ch.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
        msg
    });

    let client = Endpoint::client(localhost(), a).unwrap();
    let conn = client
        .connect(addr, PeerPolicy::Exactly(fb), ALPN)
        .await
        .unwrap();
    let mut ch = FramedStream::new(conn.open_bi().await.unwrap());
    ch.send(&ControlMessage::Ping {
        nonce: 7,
        sent_us: 1,
    })
    .await
    .unwrap();
    let pong: ControlMessage = ch.expect().await.unwrap();
    conn.close(b"done");
    assert_eq!(
        pong,
        ControlMessage::Pong {
            nonce: 7,
            ping_sent_us: 1,
            received_us: 2
        }
    );
    assert_eq!(
        srv.await.unwrap(),
        ControlMessage::Ping {
            nonce: 7,
            sent_us: 1
        }
    );
    let _ = PROTOCOL_VERSION;
}

#[tokio::test]
async fn unpinned_client_is_rejected_by_server() {
    let server = Endpoint::server(
        localhost(),
        Identity::generate().unwrap(),
        PeerPolicy::pinned([]),
        ALPN,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let server_fp = server.identity().fingerprint();
    tokio::spawn(async move { while server.accept().await.is_some() {} });

    let client = Endpoint::client(localhost(), Identity::generate().unwrap()).unwrap();
    let res = client
        .connect(addr, PeerPolicy::Exactly(server_fp), ALPN)
        .await;
    // Según el momento, el rechazo llega en el handshake o al primer uso.
    match res {
        Err(_) => {}
        Ok(conn) => {
            let r = tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
                .await
                .unwrap();
            assert!(
                r.is_err()
                    || tokio::time::timeout(Duration::from_secs(5), conn.closed())
                        .await
                        .is_ok()
            );
        }
    }
}

#[tokio::test]
async fn client_rejects_server_with_unexpected_fingerprint() {
    let server = Endpoint::server(
        localhost(),
        Identity::generate().unwrap(),
        PeerPolicy::AnyForPairing,
        ALPN,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move { while server.accept().await.is_some() {} });
    let client = Endpoint::client(localhost(), Identity::generate().unwrap()).unwrap();
    let wrong = Identity::generate().unwrap().fingerprint();
    assert!(client
        .connect(addr, PeerPolicy::Exactly(wrong), ALPN)
        .await
        .is_err());
}

#[tokio::test]
async fn video_frames_over_datagrams_with_fec() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (fa, fb) = (a.fingerprint(), b.fingerprint());
    let server = Endpoint::server(localhost(), b, PeerPolicy::pinned([fa]), ALPN).unwrap();
    let addr = server.local_addr().unwrap();
    const FRAMES: u32 = 30;

    let receiver = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().unwrap();
        let mut r = Reassembler::new(1);
        let mut frames = Vec::new();
        let mut lost = 0;
        'outer: loop {
            let d = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
                .await
                .unwrap()
                .unwrap();
            for ev in r.push(&d).unwrap() {
                let last = match ev {
                    ReassemblyEvent::Frame(f) => {
                        let s = f.frame_seq;
                        frames.push(f);
                        s
                    }
                    ReassemblyEvent::Lost { frame_seq } => {
                        lost += 1;
                        frame_seq
                    }
                };
                if last == FRAMES - 1 {
                    break 'outer;
                }
            }
        }
        // En loopback no debería perderse nada (y si se pierde, FEC lo cubre).
        assert_eq!(
            lost,
            0,
            "frames perdidos en loopback; stats={:?}",
            r.stats()
        );
        frames
    });

    let client = Endpoint::client(localhost(), a).unwrap();
    let conn = client
        .connect(addr, PeerPolicy::Exactly(fb), ALPN)
        .await
        .unwrap();
    let max = conn.max_datagram_size().expect("datagramas soportados");
    let p = Packetizer::new(
        1,
        PacketizerConfig {
            max_datagram: max,
            fec_ratio: 0.1,
        },
    );
    let mut sent = Vec::new();
    for seq in 0..FRAMES {
        let frame: Vec<u8> = (0..(20_000 + seq as usize * 500))
            .map(|i| (i as u8) ^ (seq as u8))
            .collect();
        for d in p
            .packetize(
                FrameMeta {
                    frame_seq: seq,
                    pts_us: seq * 16_666,
                    keyframe: seq == 0,
                    discardable: false,
                },
                &frame,
            )
            .unwrap()
        {
            conn.send_datagram(Bytes::from(d)).unwrap();
        }
        sent.push(frame);
        // Pacing grosero: deja drenar el buffer de envío.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let frames = receiver.await.unwrap();
    for f in &frames {
        assert_eq!(f.data, sent[f.frame_seq as usize]);
    }
    conn.close(b"done");
}
