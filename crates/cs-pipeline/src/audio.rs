//! Envío del audio del sistema (ADR-0008), común a Windows y Linux.
//!
//! Solo se crea cuando el emisor tiene el audio activado **y** el receptor lo
//! pide (`AudioRequest`). Un hilo captura tramas de 10 ms, las codifica en
//! Opus y las envía como datagramas de audio por la conexión de la sesión.

use cs_protocol::audio::AudioHeader;
use cs_protocol::control::{AudioCodec, AudioStart};
use cs_transport::Connection;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Identificador del flujo de audio de la sesión.
pub const AUDIO_STREAM_ID: u16 = 1;

/// Mensaje `AudioStart` que describe el flujo que se envía.
pub fn audio_start() -> AudioStart {
    AudioStart {
        stream_id: AUDIO_STREAM_ID,
        codec: AudioCodec::Opus,
        sample_rate: cs_audio::SAMPLE_RATE,
        channels: cs_audio::CHANNELS as u8,
        frame_ms: cs_audio::FRAME_MS as u8,
    }
}

/// Hilo de captura → Opus → datagramas. Se detiene al soltarlo.
pub struct AudioSender {
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for AudioSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSender")
            .field("sent", &self.sent.load(Ordering::Relaxed))
            .finish()
    }
}

impl AudioSender {
    /// Abre la captura del audio del sistema y empieza a enviar por `conn`.
    pub fn start(conn: Connection) -> anyhow::Result<Self> {
        let capture = cs_audio::start_capture()?;
        let mut encoder = cs_audio::codec::Encoder::new()?;
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (s, n) = (stop.clone(), sent.clone());
        let join = std::thread::Builder::new()
            .name("cs-audio-envio".into())
            .spawn(move || {
                let mut seq = 0u32;
                let mut pts = 0u32;
                while !s.load(Ordering::Relaxed) {
                    let Some(frame) = capture.recv_timeout(Duration::from_millis(100)) else {
                        continue;
                    };
                    let packet = match encoder.encode(&frame) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::debug!("audio: {e}");
                            continue;
                        }
                    };
                    let header = AudioHeader {
                        stream_id: AUDIO_STREAM_ID,
                        seq,
                        pts,
                    };
                    if let Ok(d) = header.encode(packet) {
                        if conn.send_datagram(d.into()).is_ok() {
                            n.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    seq = seq.wrapping_add(1);
                    pts = pts.wrapping_add(cs_audio::FRAME_SAMPLES as u32);
                }
            })?;
        tracing::info!("audio del sistema: enviando (Opus 48 kHz estéreo)");
        Ok(Self {
            stop,
            sent,
            join: Some(join),
        })
    }

    /// Tramas de audio enviadas.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
