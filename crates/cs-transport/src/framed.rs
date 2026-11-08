//! Canal de mensajes enmarcados (prefijo de longitud + postcard) sobre un
//! stream bidireccional QUIC.

use crate::TransportError;
use cs_protocol::framing::{self, FrameDecoder};
use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug)]
pub struct FramedStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
    buf: Box<[u8]>,
}

impl FramedStream {
    pub fn new((send, recv): (quinn::SendStream, quinn::RecvStream)) -> Self {
        FramedStream {
            send,
            recv,
            decoder: FrameDecoder::new(),
            buf: vec![0u8; 16 * 1024].into_boxed_slice(),
        }
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), TransportError> {
        let bytes = framing::encode(msg)?;
        self.send.write_all(&bytes).await?;
        Ok(())
    }

    /// Siguiente mensaje; `Ok(None)` si el par cerró el stream limpiamente.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>, TransportError> {
        loop {
            if let Some(msg) = self.decoder.next_frame()? {
                return Ok(Some(msg));
            }
            match self.recv.read(&mut self.buf).await? {
                Some(n) => self.decoder.push(&self.buf[..n]),
                None => return Ok(None),
            }
        }
    }

    /// Como [`recv`](Self::recv) pero trata el fin de stream como error.
    pub async fn expect<T: DeserializeOwned>(&mut self) -> Result<T, TransportError> {
        self.recv().await?.ok_or(TransportError::UnexpectedEof)
    }

    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }
}
