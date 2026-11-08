//! JSON por líneas con límite de tamaño.

use crate::protocol::MAX_LINE_BYTES;
use crate::IpcClientError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Lee un mensaje. `Ok(None)` = fin de flujo limpio.
pub async fn read_message<R, T>(r: &mut R) -> Result<Option<T>, IpcClientError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(if buf.len() > MAX_LINE_BYTES {
            IpcClientError::Protocol("mensaje demasiado grande".into())
        } else {
            IpcClientError::Protocol("flujo truncado".into())
        });
    }
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| IpcClientError::Protocol(format!("JSON inválido: {e}")))
}

/// Escribe un mensaje seguido de `\n`.
pub async fn write_message<W, T>(w: &mut W, msg: &T) -> Result<(), IpcClientError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(msg)
        .map_err(|e| IpcClientError::Protocol(format!("serialización: {e}")))?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMessage, Request};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn write_then_read() {
        let (a, b) = tokio::io::duplex(1024);
        let (_ra, mut wa) = tokio::io::split(a);
        let msg = ClientMessage::Request {
            id: 3,
            request: Request::ListPeers,
        };
        write_message(&mut wa, &msg).await.unwrap();
        drop(wa);
        let mut rb = BufReader::new(b);
        let got: ClientMessage = read_message(&mut rb).await.unwrap().unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn oversized_line_is_rejected() {
        let big = vec![b'a'; MAX_LINE_BYTES + 10];
        let mut r = BufReader::new(&big[..]);
        let res: Result<Option<ClientMessage>, _> = read_message(&mut r).await;
        assert!(matches!(res, Err(IpcClientError::Protocol(_))));
    }

    #[tokio::test]
    async fn garbage_is_an_error() {
        let mut r = BufReader::new(&b"{nope\n"[..]);
        let res: Result<Option<ClientMessage>, _> = read_message(&mut r).await;
        assert!(res.is_err());
    }
}
