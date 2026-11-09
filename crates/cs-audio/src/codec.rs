//! Opus (48 kHz estéreo, tramas de 10 ms) con `opus1`, en Rust puro.

use crate::{AudioError, FRAME_LEN, FRAME_SAMPLES};
use std::num::NonZeroU32;

/// Tasa objetivo: transparente para música en estéreo.
pub const BITRATE_BPS: u32 = 128_000;
/// Tope de un paquete Opus (RFC 6716).
const MAX_PACKET: usize = 1275;

fn err(e: impl std::fmt::Debug) -> AudioError {
    AudioError::Codec(format!("{e:?}"))
}

/// Codificador de tramas de [`FRAME_LEN`] valores intercalados.
pub struct Encoder {
    inner: opus1::OpusEncoder,
    out: Vec<u8>,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Encoder")
    }
}

impl Encoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut inner = opus1::OpusEncoder::new(
            opus1::SampleRate::Hz48000,
            opus1::Channels::Stereo,
            opus1::Application::Audio,
        )
        .map_err(err)?;
        if let Some(b) = NonZeroU32::new(BITRATE_BPS) {
            inner.set_bitrate(opus1::Bitrate::BitsPerSecond(b));
        }
        Ok(Self {
            inner,
            out: vec![0; MAX_PACKET],
        })
    }

    /// Codifica una trama completa y devuelve el paquete Opus.
    pub fn encode(&mut self, frame: &[f32]) -> Result<&[u8], AudioError> {
        if frame.len() != FRAME_LEN {
            return Err(AudioError::Codec(format!(
                "trama de {} valores (se esperaban {FRAME_LEN})",
                frame.len()
            )));
        }
        let n = self
            .inner
            .encode(frame, FRAME_SAMPLES, &mut self.out)
            .map_err(err)?;
        Ok(&self.out[..n])
    }
}

/// Decodificador con ocultación de pérdidas.
pub struct Decoder {
    inner: opus1::OpusDecoder,
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Decoder")
    }
}

impl Decoder {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            inner: opus1::OpusDecoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Stereo)
                .map_err(err)?,
        })
    }

    /// Decodifica un paquete; añade la trama a `out`.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError> {
        self.decode_into(packet, out)
    }

    /// Trama sustituta para un paquete perdido (ocultación de Opus).
    pub fn conceal(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError> {
        self.decode_into(&[], out)
    }

    fn decode_into(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError> {
        let start = out.len();
        out.resize(start + FRAME_LEN, 0.0);
        match self.inner.decode(packet, FRAME_SAMPLES, &mut out[start..]) {
            Ok(n) => {
                out.truncate(start + n * crate::CHANNELS);
                Ok(())
            }
            Err(e) => {
                out.truncate(start);
                Err(err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .flat_map(|n| {
                let v = 0.3 * (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 48_000.0).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn roundtrip_and_concealment() {
        let pcm = tone(50);
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = Vec::new();
        let mut bytes = 0;
        for (i, f) in pcm.chunks(FRAME_LEN).enumerate() {
            let p = enc.encode(f).unwrap().to_vec();
            bytes += p.len();
            if i == 30 {
                dec.conceal(&mut out).unwrap(); // paquete perdido
            } else {
                dec.decode(&p, &mut out).unwrap();
            }
        }
        assert_eq!(out.len(), pcm.len());
        // ~128 kbps: 50 tramas de 10 ms ≈ 0,5 s ≈ 8 KB.
        assert!((4_000..16_000).contains(&bytes), "{bytes} B");
        // Hay señal (no silencio) tras el retardo del códec.
        let energy: f32 = out[FRAME_LEN * 5..].iter().map(|v| v * v).sum();
        assert!(energy > 100.0, "energía {energy}");
    }

    #[test]
    fn rejects_wrong_frame_size() {
        let mut enc = Encoder::new().unwrap();
        assert!(enc.encode(&[0.0; 10]).is_err());
    }
}
