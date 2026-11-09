//! Transferencia de archivos sobre QUIC real en loopback.

use cs_files::{FileEvent, FileService, FilesHandle};
use cs_protocol::files::FileHeader;
use cs_protocol::ALPN;
use cs_security::identity::Identity;
use cs_security::verifier::PeerPolicy;
use cs_transport::{Connection, Endpoint};
use std::path::PathBuf;
use std::time::Duration;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cs-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Dos conexiones QUIC emparejadas (cliente, servidor).
async fn pair() -> (Connection, Connection, Endpoint, Endpoint) {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (fa, fb) = (a.fingerprint(), b.fingerprint());
    let server = Endpoint::server(
        "127.0.0.1:0".parse().unwrap(),
        b,
        PeerPolicy::pinned([fa]),
        ALPN,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let client = Endpoint::client("127.0.0.1:0".parse().unwrap(), a).unwrap();
    let srv = server.clone();
    let accept = tokio::spawn(async move { srv.accept().await.unwrap().unwrap() });
    let c = client
        .connect(addr, PeerPolicy::Exactly(fb), ALPN)
        .await
        .unwrap();
    let s = accept.await.unwrap();
    (c, s, client, server)
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<FileEvent>) -> FileEvent {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("evento a tiempo")
        .expect("canal abierto")
}

#[tokio::test]
async fn files_travel_both_ways_and_never_overwrite() {
    let (c, s, _ce, _se) = pair().await;
    let (out_a, in_a, in_b) = (tmp("out-a"), tmp("in-a"), tmp("in-b"));
    let (ha, tx_a, mut ev_a) = FilesHandle::new(in_a.clone());
    let (hb, tx_b, mut ev_b) = FilesHandle::new(in_b.clone());
    let sa = FileService::start(c, &ha);
    let sb = FileService::start(s, &hb);
    sa.set_peer_enabled(true);
    sb.set_peer_enabled(true);

    // 700 KB pseudoaleatorios de A a B, dos veces (la segunda no sobrescribe).
    let data: Vec<u8> = (0..700_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let src = out_a.join("datos ñ.bin");
    std::fs::write(&src, &data).unwrap();
    tx_a.send(src.clone()).unwrap();
    tx_a.send(src.clone()).unwrap();
    let mut received = Vec::new();
    for _ in 0..2 {
        match next_event(&mut ev_a).await {
            FileEvent::Sent { name, size } => {
                assert_eq!(name, "datos ñ.bin");
                assert_eq!(size, data.len() as u64);
            }
            other => panic!("esperaba Sent: {other:?}"),
        }
        match next_event(&mut ev_b).await {
            FileEvent::Received { path, size, .. } => {
                assert_eq!(size, data.len() as u64);
                received.push(path);
            }
            other => panic!("esperaba Received: {other:?}"),
        }
    }
    assert_eq!(received[0], in_b.join("datos ñ.bin"));
    assert_eq!(received[1], in_b.join("datos ñ (1).bin"));
    for p in &received {
        assert_eq!(std::fs::read(p).unwrap(), data);
    }

    // Y de B a A.
    let src_b = in_b.join("nota.txt");
    std::fs::write(&src_b, "hola").unwrap();
    tx_b.send(src_b).unwrap();
    assert!(matches!(
        next_event(&mut ev_b).await,
        FileEvent::Sent { .. }
    ));
    match next_event(&mut ev_a).await {
        FileEvent::Received { path, .. } => {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "hola")
        }
        other => panic!("esperaba Received: {other:?}"),
    }
    // Sin restos .part.
    let leftovers: Vec<_> = std::fs::read_dir(&in_b)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn nothing_is_sent_until_the_peer_enables_files() {
    let (c, s, _ce, _se) = pair().await;
    let (ha, tx_a, mut ev_a) = FilesHandle::new(tmp("in-a2"));
    let (hb, _tx_b, _ev_b) = FilesHandle::new(tmp("in-b2"));
    let _sa = FileService::start(c, &ha);
    let _sb = FileService::start(s, &hb);
    let src = tmp("out-a2").join("x.txt");
    std::fs::write(&src, "x").unwrap();
    tx_a.send(src).unwrap();
    match next_event(&mut ev_a).await {
        FileEvent::Failed {
            incoming: false,
            error,
            ..
        } => assert!(error.contains("no tiene activada"), "{error}"),
        other => panic!("esperaba Failed: {other:?}"),
    }
}

#[tokio::test]
async fn hostile_names_and_short_bodies_are_contained() {
    let (c, s, _ce, _se) = pair().await;
    let inbox = tmp("in-hostile");
    let (hb, _tx, mut ev_b) = FilesHandle::new(inbox.clone());
    let _sb = FileService::start(s, &hb);

    // Ruta con `..`: se guarda solo el último componente, dentro del buzón.
    let mut send = c.open_uni().await.unwrap();
    let h = FileHeader {
        name: "../../fuera.txt".into(),
        size: 3,
    };
    send.write_all(&h.encode().unwrap()).await.unwrap();
    send.write_all(b"abc").await.unwrap();
    send.finish().unwrap();
    match next_event(&mut ev_b).await {
        FileEvent::Received { path, .. } => assert_eq!(path, inbox.join("fuera.txt")),
        other => panic!("esperaba Received: {other:?}"),
    }

    // Anuncia 10 bytes y manda 4: se descarta sin dejar nada.
    let mut send = c.open_uni().await.unwrap();
    let h = FileHeader {
        name: "corto.bin".into(),
        size: 10,
    };
    send.write_all(&h.encode().unwrap()).await.unwrap();
    send.write_all(b"abcd").await.unwrap();
    send.finish().unwrap();
    match next_event(&mut ev_b).await {
        FileEvent::Failed {
            incoming: true,
            error,
            ..
        } => assert!(error.contains("incompleto"), "{error}"),
        other => panic!("esperaba Failed: {other:?}"),
    }
    let names: Vec<String> = std::fs::read_dir(&inbox)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["fuera.txt".to_string()]);
}

#[tokio::test]
async fn a_file_queued_just_before_the_peer_announces_still_goes() {
    let (c, s, _ce, _se) = pair().await;
    let (ha, tx_a, mut ev_a) = FilesHandle::new(tmp("in-a3"));
    let (hb, _tx_b, mut ev_b) = FilesHandle::new(tmp("in-b3"));
    let sa = FileService::start(c, &ha);
    let _sb = FileService::start(s, &hb);
    let src = tmp("out-a3").join("pronto.txt");
    std::fs::write(&src, "pronto").unwrap();
    tx_a.send(src).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    sa.set_peer_enabled(true);
    assert!(matches!(
        next_event(&mut ev_a).await,
        FileEvent::Sent { .. }
    ));
    assert!(matches!(
        next_event(&mut ev_b).await,
        FileEvent::Received { .. }
    ));
}
