//! Prueba de integración con el transporte real del sistema (named pipe en
//! Windows, socket Unix en Linux): un cliente se conecta, hace peticiones,
//! recibe eventos y se comprueba la instancia única.

use cs_ipc::{
    serve_connection, Endpoint, ErrorCode, Event, Handler, HandlerFuture, IpcClient,
    IpcClientError, IpcError, Listener, PeerInfo, Request, Response,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

struct TestHandler;

impl Handler for TestHandler {
    fn handle(self: Arc<Self>, request: Request) -> HandlerFuture {
        Box::pin(async move {
            match request {
                Request::ListPeers => Ok(Response::Peers {
                    peers: vec![PeerInfo {
                        name: "par de prueba".into(),
                        fingerprint: "ab".repeat(32),
                        fingerprint_short: "abab-abab-abab-abab".into(),
                        paired_at: 1,
                    }],
                }),
                Request::PairWith { .. } => {
                    // Petición lenta: no debe bloquear a las demás.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(Response::Ok)
                }
                _ => Err(IpcError::new(ErrorCode::Unsupported, "no en el test")),
            }
        })
    }
}

fn unique_endpoint() -> Endpoint {
    let n = std::process::id();
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    #[cfg(windows)]
    return Endpoint::custom(format!(r"\\.\pipe\crossscreen-ipc-test-{n}-{t}"));
    #[cfg(unix)]
    {
        let dir = std::env::temp_dir().join(format!("cs-ipc-test-{n}-{t}"));
        Endpoint::custom(dir.join("agent.sock").to_string_lossy().into_owned())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_response_and_events_over_real_transport() {
    let ep = unique_endpoint();
    let mut listener = Listener::bind(&ep).await.expect("bind");

    // Instancia única: un segundo bind con el mismo nombre falla.
    let second = Listener::bind(&ep).await;
    let e = second.expect_err("el segundo bind debe fallar");
    assert!(
        cs_ipc::transport::is_name_in_use(&e),
        "error inesperado: {e:?}"
    );

    let (ev_tx, _) = broadcast::channel::<Event>(16);
    let handler = Arc::new(TestHandler);
    let server = {
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let stream = listener.accept().await.expect("accept");
                let h = handler.clone();
                let events = ev_tx.subscribe();
                tokio::spawn(serve_connection(stream, h, events, "test-agent"));
            }
        })
    };

    let (client, mut events) = IpcClient::connect(&ep, "test").await.expect("connect");
    assert_eq!(client.agent_version(), "test-agent");

    // Petición lenta y rápida en paralelo: la rápida termina primero.
    let client = Arc::new(client);
    let slow = {
        let c = client.clone();
        tokio::spawn(async move {
            c.request(Request::PairWith {
                addr: "127.0.0.1:1".into(),
                pin: "000000".into(),
            })
            .await
        })
    };
    let peers = client.request(Request::ListPeers).await.expect("list");
    match peers {
        Response::Peers { peers } => assert_eq!(peers[0].name, "par de prueba"),
        other => panic!("respuesta inesperada {other:?}"),
    }
    assert!(
        !slow.is_finished(),
        "la petición lenta no debería haber terminado"
    );
    assert_eq!(slow.await.unwrap().unwrap(), Response::Ok);

    // Error del agente tipado.
    match client.request(Request::ListDisplays).await {
        Err(IpcClientError::Agent(e)) => assert_eq!(e.code, ErrorCode::Unsupported),
        other => panic!("se esperaba error del agente: {other:?}"),
    }

    // Eventos difundidos.
    ev_tx.send(Event::PeersChanged).unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("evento a tiempo")
        .expect("canal abierto");
    assert_eq!(got, Event::PeersChanged);

    // Un segundo cliente simultáneo también funciona.
    let (client2, _ev2) = IpcClient::connect(&ep, "test2").await.expect("connect 2");
    assert!(matches!(
        client2.request(Request::ListPeers).await,
        Ok(Response::Peers { .. })
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn connecting_without_agent_fails_cleanly() {
    let ep = unique_endpoint();
    let r = IpcClient::connect(&ep, "test").await;
    assert!(matches!(r, Err(IpcClientError::Io(_))), "{r:?}");
}
