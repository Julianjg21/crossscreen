//! Servidor IPC: atiende una conexión ya autenticada por el transporte.

use crate::framing::{read_message, write_message};
use crate::protocol::{
    AgentMessage, ClientMessage, Event, IpcError, Request, Response, IPC_VERSION,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc};

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, IpcError>> + Send>>;

/// Lógica del agente: resuelve cada petición.
pub trait Handler: Send + Sync + 'static {
    fn handle(self: Arc<Self>, request: Request) -> HandlerFuture;
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Atiende una conexión hasta que el cliente la cierre. Las peticiones se
/// resuelven en paralelo; los eventos de `events` se reenvían al cliente.
pub async fn serve_connection<S, H>(
    stream: S,
    handler: Arc<H>,
    mut events: broadcast::Receiver<Event>,
    agent_version: &str,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
    H: Handler,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);

    // Saludo con versión.
    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_message::<_, ClientMessage>(&mut r)).await;
    let client = match hello {
        Ok(Ok(Some(ClientMessage::Hello { version, client }))) => {
            if version.major != IPC_VERSION.major {
                let _ = write_message(
                    &mut w,
                    &AgentMessage::Rejected {
                        reason: format!(
                            "versión de protocolo incompatible: cliente v{}.{}, agente v{}.{}",
                            version.major, version.minor, IPC_VERSION.major, IPC_VERSION.minor
                        ),
                    },
                )
                .await;
                return;
            }
            client
        }
        other => {
            tracing::debug!("IPC: saludo inválido o ausente: {other:?}");
            return;
        }
    };
    if write_message(
        &mut w,
        &AgentMessage::Welcome {
            version: IPC_VERSION,
            agent_version: agent_version.to_owned(),
        },
    )
    .await
    .is_err()
    {
        return;
    }
    tracing::info!(client, "cliente IPC conectado");

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_message(&mut w, &msg).await.is_err() {
                break;
            }
        }
    });
    let forwarder = {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if tx.send(AgentMessage::Event { event }).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("IPC: cliente lento, {n} eventos descartados");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    loop {
        match read_message::<_, ClientMessage>(&mut r).await {
            Ok(Some(ClientMessage::Request { id, request })) => {
                tracing::debug!(id, ?request, "petición IPC");
                let handler = handler.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let result = handler.handle(request).await;
                    let _ = tx.send(AgentMessage::Response { id, result });
                });
            }
            Ok(Some(ClientMessage::Hello { .. })) => {
                tracing::debug!("IPC: saludo repetido ignorado");
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("IPC: conexión cerrada por mensaje inválido: {e}");
                break;
            }
        }
    }
    forwarder.abort();
    drop(tx);
    // Deja que se envíen las respuestas en curso; el escritor termina cuando
    // todas las tareas de petición sueltan su `tx`.
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    tracing::info!(client, "cliente IPC desconectado");
}
