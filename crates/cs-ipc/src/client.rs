//! Cliente IPC (lo usan la UI y `crossscreen-agent --probe`).

use crate::framing::{read_message, write_message};
use crate::protocol::{
    AgentMessage, ClientMessage, Event, IpcError, Request, Response, IPC_VERSION,
};
use crate::transport::{self, Endpoint};
use crate::IpcClientError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::{mpsc, oneshot};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, IpcError>>>>>;

/// Tiempo máximo de cualquier petición. El agente aplica sus propios
/// límites (p. ej. 30 s para emparejar); esto solo evita colgarse para siempre.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Conexión con el agente. Las peticiones pueden hacerse concurrentemente.
#[derive(Debug)]
pub struct IpcClient {
    tx: mpsc::UnboundedSender<ClientMessage>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    agent_version: String,
}

impl IpcClient {
    /// Conecta, verifica el propietario del punto de conexión y negocia la
    /// versión. Devuelve el cliente y el canal de eventos del agente.
    pub async fn connect(
        ep: &Endpoint,
        client_name: &str,
    ) -> Result<(IpcClient, mpsc::UnboundedReceiver<Event>), IpcClientError> {
        let stream = transport::connect(ep).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut r = BufReader::new(r);

        write_message(
            &mut w,
            &ClientMessage::Hello {
                version: IPC_VERSION,
                client: client_name.to_owned(),
            },
        )
        .await?;
        let welcome =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, read_message::<_, AgentMessage>(&mut r))
                .await
                .map_err(|_| {
                    IpcClientError::Protocol("el agente no respondió al saludo".into())
                })??;
        let agent_version = match welcome {
            Some(AgentMessage::Welcome {
                version,
                agent_version,
            }) if version.major == IPC_VERSION.major => agent_version,
            Some(AgentMessage::Welcome { version, .. }) => {
                return Err(IpcClientError::VersionMismatch(format!(
                    "agente v{}.{}, cliente v{}.{}",
                    version.major, version.minor, IPC_VERSION.major, IPC_VERSION.minor
                )))
            }
            Some(AgentMessage::Rejected { reason }) => {
                return Err(IpcClientError::VersionMismatch(reason))
            }
            Some(_) => return Err(IpcClientError::Protocol("se esperaba Welcome".into())),
            None => return Err(IpcClientError::Disconnected),
        };

        let (tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Event>();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));

        // Escritor.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = write_message(&mut w, &msg).await {
                    tracing::debug!("IPC: escritura fallida: {e}");
                    break;
                }
            }
        });

        // Lector: reparte respuestas y eventos.
        {
            let pending = pending.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                loop {
                    match read_message::<_, AgentMessage>(&mut r).await {
                        Ok(Some(AgentMessage::Response { id, result })) => {
                            let waiter = pending.lock().ok().and_then(|mut p| p.remove(&id));
                            if let Some(w) = waiter {
                                let _ = w.send(result);
                            }
                        }
                        Ok(Some(AgentMessage::Event { event })) => {
                            let _ = ev_tx.send(event);
                        }
                        Ok(Some(other)) => tracing::debug!("IPC: mensaje inesperado {other:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!("IPC: lectura fallida: {e}");
                            break;
                        }
                    }
                }
                closed.store(true, Ordering::SeqCst);
                // Al soltar los emisores, las peticiones pendientes ven `Disconnected`.
                if let Ok(mut p) = pending.lock() {
                    p.clear();
                }
            });
        }

        Ok((
            IpcClient {
                tx,
                pending,
                next_id: AtomicU64::new(1),
                closed,
                agent_version,
            },
            ev_rx,
        ))
    }

    pub fn agent_version(&self) -> &str {
        &self.agent_version
    }

    /// `true` si la conexión con el agente se cerró.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.tx.is_closed()
    }

    pub async fn request(&self, request: Request) -> Result<Response, IpcClientError> {
        if self.is_closed() {
            return Err(IpcClientError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| IpcClientError::Disconnected)?
            .insert(id, otx);
        // El lector marca `closed` antes de vaciar `pending`: si se cerró justo
        // ahora, nuestra entrada podría no vaciarse nunca.
        if self.closed.load(Ordering::SeqCst) {
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&id);
            }
            return Err(IpcClientError::Disconnected);
        }
        if self
            .tx
            .send(ClientMessage::Request { id, request })
            .is_err()
        {
            return Err(IpcClientError::Disconnected);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, orx).await {
            Ok(Ok(Ok(resp))) => Ok(resp),
            Ok(Ok(Err(e))) => Err(IpcClientError::Agent(e)),
            Ok(Err(_)) => Err(IpcClientError::Disconnected),
            Err(_) => {
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&id);
                }
                Err(IpcClientError::Protocol(
                    "el agente no respondió a tiempo".into(),
                ))
            }
        }
    }
}
