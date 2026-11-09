//! Conversión entre el formato del dispositivo y el interno (48 kHz estéreo).
//!
//! - [`to_stereo`] / [`from_stereo`]: número de canales (los dispositivos
//!   pueden ser mono, 5.1 o 7.1: se toman o rellenan los canales frontales).
//! - [`Resampler`]: frecuencia de muestreo (p. ej. 44,1 kHz ↔ 48 kHz) con
//!   `rubato` (FFT síncrona); a la misma frecuencia no hace nada.
//! - [`FrameAssembler`]: corta un flujo continuo en tramas de 10 ms.

use crate::{AudioError, CHANNELS, FRAME_LEN};
use rubato::{FftFixedIn, Resampler as _};

/// `input` intercalado con `channels` canales → estéreo intercalado
/// (mono se duplica; con más de dos canales se toman los frontales L y R).
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(input.iter().flat_map(|&v| [v, v])),
        _ => out.extend(input.chunks_exact(channels).flat_map(|f| [f[0], f[1]])),
    }
}

/// Estéreo intercalado → `channels` canales (mono = media; con más de dos
/// canales, L y R en los frontales y silencio en el resto).
pub fn from_stereo(stereo: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(stereo.chunks_exact(2).map(|f| 0.5 * (f[0] + f[1]))),
        _ => {
            for f in stereo.chunks_exact(2) {
                out.push(f[0]);
                out.push(f[1]);
                out.extend(std::iter::repeat_n(0.0, channels - 2));
            }
        }
    }
}

/// Bloque de entrada del remuestreador (en muestras por canal).
const CHUNK: usize = 480;

/// Remuestreador estéreo de `rate_in` a `rate_out`.
pub struct Resampler {
    inner: Option<FftFixedIn<f32>>,
    pending: [Vec<f32>; CHANNELS],
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("active", &self.inner.is_some())
            .finish()
    }
}

impl Resampler {
    pub fn new(rate_in: u32, rate_out: u32) -> Result<Self, AudioError> {
        let inner = if rate_in == rate_out {
            None
        } else {
            Some(
                FftFixedIn::new(rate_in as usize, rate_out as usize, CHUNK, 2, CHANNELS).map_err(
                    |e| AudioError::Device(format!("remuestreo {rate_in}→{rate_out}: {e}")),
                )?,
            )
        };
        Ok(Self {
            inner,
            pending: [Vec::new(), Vec::new()],
        })
    }

    /// Añade a `out` el resultado de remuestrear `stereo` (intercalado). Lo
    /// que no llena un bloque queda pendiente para la siguiente llamada.
    pub fn process(&mut self, stereo: &[f32], out: &mut Vec<f32>) {
        let Some(r) = self.inner.as_mut() else {
            out.extend_from_slice(stereo);
            return;
        };
        for f in stereo.chunks_exact(2) {
            self.pending[0].push(f[0]);
            self.pending[1].push(f[1]);
        }
        while self.pending[0].len() >= CHUNK {
            let block = [
                self.pending[0][..CHUNK].to_vec(),
                self.pending[1][..CHUNK].to_vec(),
            ];
            self.pending[0].drain(..CHUNK);
            self.pending[1].drain(..CHUNK);
            match r.process(&block, None) {
                Ok(res) => {
                    for (l, rr) in res[0].iter().zip(res[1].iter()) {
                        out.push(*l);
                        out.push(*rr);
                    }
                }
                Err(e) => tracing::debug!("remuestreo: {e}"),
            }
        }
    }
}

/// Acumula estéreo intercalado y entrega tramas completas de [`FRAME_LEN`].
#[derive(Debug, Default)]
pub struct FrameAssembler {
    buf: Vec<f32>,
}

impl FrameAssembler {
    pub fn push(&mut self, stereo: &[f32]) {
        self.buf.extend_from_slice(stereo);
    }

    /// Siguiente trama completa, si la hay.
    pub fn next_frame(&mut self) -> Option<Vec<f32>> {
        (self.buf.len() >= FRAME_LEN).then(|| self.buf.drain(..FRAME_LEN).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_conversion() {
        let mut out = Vec::new();
        to_stereo(&[0.5, -0.5], 1, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        // 5.1: FL FR C LFE SL SR → FL FR.
        to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, &mut out);
        assert_eq!(out, [1.0, 2.0]);
        out.clear();
        from_stereo(&[1.0, 2.0], 6, &mut out);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 0.0, 0.0]);
        out.clear();
        from_stereo(&[1.0, 3.0], 1, &mut out);
        assert_eq!(out, [2.0]);
    }

    #[test]
    fn resample_44100_to_48000_keeps_duration_and_tone() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let secs = 1.0;
        let n_in = (44_100.0 * secs) as usize;
        let input: Vec<f32> = (0..n_in)
            .flat_map(|n| {
                let v = (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 44_100.0).sin();
                [v, v]
            })
            .collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let n_out = out.len() / 2;
        // ~48 000 muestras (menos lo que queda en el último bloque y el retardo).
        assert!((46_500..=48_000).contains(&n_out), "{n_out}");
        // La señal sigue siendo un tono de amplitud ~1 (no ruido ni silencio).
        let peak = out[20_000..40_000].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((0.9..1.1).contains(&peak), "pico {peak}");
    }

    #[test]
    fn passthrough_and_frames() {
        let mut r = Resampler::new(48_000, 48_000).unwrap();
        let mut out = Vec::new();
        r.process(&[1.0, 2.0], &mut out);
        assert_eq!(out, [1.0, 2.0]);
        let mut fa = FrameAssembler::default();
        fa.push(&vec![0.0; FRAME_LEN + 10]);
        assert_eq!(fa.next_frame().map(|f| f.len()), Some(FRAME_LEN));
        assert!(fa.next_frame().is_none());
    }
}
