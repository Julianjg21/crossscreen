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
    unknown_skipped: u64,
}

impl FramedStream {
    pub fn new((send, recv): (quinn::SendStream, quinn::RecvStream)) -> Self {
        FramedStream {
            send,
            recv,
            decoder: FrameDecoder::new(),
            buf: vec![0u8; 16 * 1024].into_boxed_slice(),
            unknown_skipped: 0,
        }
    }

    /// Mensajes recibidos que no se pudieron interpretar y se ignoraron.
    pub fn unknown_skipped(&self) -> u64 {
        self.unknown_skipped
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), TransportError> {
        let bytes = framing::encode(msg)?;
        self.send.write_all(&bytes).await?;
        Ok(())
    }

    /// Siguiente mensaje; `Ok(None)` si el par cerró el stream limpiamente.
    ///
    /// Los mensajes que no se pueden interpretar (p. ej. variantes nuevas de
    /// un par con una versión posterior del protocolo) **se ignoran** con un
    /// aviso en el log, en lugar de cortar la sesión.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>, TransportError> {
        loop {
            match self.decoder.next_frame() {
                Ok(Some(msg)) => return Ok(Some(msg)),
                Ok(None) => {}
                Err(framing::FramingError::UnknownMessage { len, source }) => {
                    self.unknown_skipped += 1;
                    if self.unknown_skipped <= 5 || self.unknown_skipped.is_power_of_two() {
                        tracing::warn!(
                            len,
                            total = self.unknown_skipped,
                            "mensaje de control desconocido ignorado ({source}); ¿versiones distintas en cada equipo?"
                        );
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
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
