//! Búfer de reproducción (*jitter buffer*).
//!
//! El hilo de red mete paquetes Opus con [`JitterBuffer::push`] (los
//! decodifica, oculta los huecos y descarta los tardíos) y el dispositivo de
//! audio saca bloques con [`JitterBuffer::pull`]. Antes de sonar acumula el
//! objetivo (al principio [`TARGET_MS`]); si se vacía, vuelve a acumular
//! (silencio mientras tanto) y **sube el objetivo** [`STEP_MS`] hasta
//! [`MAX_TARGET_MS`]; tras [`CALM_SECS`] sin vaciarse lo baja poco a poco.
//! Así empieza con poca latencia y solo crece si la red lo necesita. Si
//! acumula más de [`MAX_EXTRA_MS`] por encima del objetivo, recorta.

use crate::codec::Decoder;
use crate::{AudioError, CHANNELS, SAMPLE_RATE};
use std::collections::VecDeque;

/// Latencia objetivo inicial (y mínima) del búfer.
pub const TARGET_MS: u32 = 60;
/// Objetivo máximo tras vaciados repetidos.
pub const MAX_TARGET_MS: u32 = 160;
/// Subida del objetivo en cada vaciado.
pub const STEP_MS: u32 = 20;
/// Tiempo sin vaciados para bajar el objetivo un paso (de 10 ms).
pub const CALM_SECS: u32 = 10;
/// Margen sobre el objetivo antes de recortar (ráfagas, deriva de relojes).
pub const MAX_EXTRA_MS: u32 = 140;
/// Compatibilidad: tope absoluto del búfer.
pub const MAX_MS: u32 = MAX_TARGET_MS + MAX_EXTRA_MS;
/// Banda alrededor del objetivo sin corrección de deriva.
pub const DRIFT_BAND_MS: u32 = 20;
/// Corrección de deriva: una trama de audio de cada `DRIFT_PERIOD` se salta
/// (si sobra) o se repite (si falta), un 0,5 % inaudible. Cubre de sobra la
/// diferencia real entre relojes de tarjetas de sonido (~0,01 %).
pub const DRIFT_PERIOD: u64 = 200;
/// Huecos más largos no se ocultan trama a trama (sería ruido): se saltan.
const MAX_CONCEAL_FRAMES: u32 = 5;

const fn ms_to_values(ms: u32) -> usize {
    (SAMPLE_RATE * ms / 1000) as usize * CHANNELS
}

/// Contadores para el resumen de la sesión.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    pub packets: u64,
    pub concealed: u64,
    pub late_or_duplicate: u64,
    pub underruns: u64,
    pub trimmed_values: u64,
    /// Tramas saltadas o repetidas para compensar la deriva de relojes.
    pub drift_adjusted: u64,
}

#[derive(Debug)]
pub struct JitterBuffer {
    decoder: Decoder,
    buf: VecDeque<f32>,
    scratch: Vec<f32>,
    next_seq: Option<u32>,
    priming: bool,
    /// Objetivo actual en ms (adaptativo).
    target_ms: u32,
    /// Valores sacados desde el último vaciado (mide el tiempo en calma).
    calm_values: u64,
    /// Contador de tramas de salida (para espaciar las correcciones de deriva).
    drift_counter: u64,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            decoder: Decoder::new()?,
            buf: VecDeque::with_capacity(ms_to_values(MAX_MS) * 2),
            scratch: Vec::new(),
            next_seq: None,
            priming: true,
            target_ms: TARGET_MS,
            calm_values: 0,
            drift_counter: 0,
            stats: JitterStats::default(),
        })
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Objetivo actual del búfer en ms.
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    /// Valores listos para sonar (intercalados).
    pub fn buffered_values(&self) -> usize {
        self.buf.len()
    }

    /// Paquete Opus con su número de secuencia.
    pub fn push(&mut self, seq: u32, packet: &[u8]) {
        if let Some(next) = self.next_seq {
            let delta = seq.wrapping_sub(next) as i32;
            if delta < 0 {
                // Llegó tarde (ya se ocultó su hueco) o repetido.
                self.stats.late_or_duplicate += 1;
                return;
            }
            let gap = delta as u32;
            if (1..=MAX_CONCEAL_FRAMES).contains(&gap) {
                for _ in 0..gap {
                    self.scratch.clear();
                    if self.decoder.conceal(&mut self.scratch).is_ok() {
                        self.buf.extend(self.scratch.iter().copied());
                        self.stats.concealed += 1;
                    }
                }
            }
        }
        self.scratch.clear();
        match self.decoder.decode(packet, &mut self.scratch) {
            Ok(()) => {
                self.buf.extend(self.scratch.iter().copied());
                self.stats.packets += 1;
            }
            Err(e) => tracing::debug!("paquete de audio no decodificable: {e}"),
        }
        self.next_seq = Some(seq.wrapping_add(1));
        let max = ms_to_values(self.target_ms + MAX_EXTRA_MS);
        if self.buf.len() > max {
            let excess = self.buf.len() - ms_to_values(self.target_ms);
            let excess = excess - excess % CHANNELS;
            self.buf.drain(..excess);
            self.stats.trimmed_values += excess as u64;
        }
    }

    /// Llena `out` (intercalado estéreo) con lo que haya; silencio si falta.
    pub fn pull(&mut self, out: &mut [f32]) {
        if self.priming && self.buf.len() < ms_to_values(self.target_ms) {
            out.fill(0.0);
            return;
        }
        self.priming = false;
        // Deriva de relojes: si el nivel se aleja del objetivo, saltar o
        // repetir alguna trama (0,5 %) para volver a él sin cortes.
        let target = ms_to_values(self.target_ms);
        let band = ms_to_values(DRIFT_BAND_MS);
        let level = self.buf.len();
        let mode: i8 = if level > target + band {
            1
        } else if level + band < target {
            -1
        } else {
            0
        };
        let frames_out = out.len() / CHANNELS;
        let mut written = 0;
        let mut consumed = 0usize;
        while written < frames_out && self.buf.len() >= CHANNELS {
            self.drift_counter += 1;
            let adjust = mode != 0 && self.drift_counter % DRIFT_PERIOD == 0;
            if adjust && mode > 0 && self.buf.len() >= 2 * CHANNELS {
                // Sobra: saltar una trama.
                self.buf.drain(..CHANNELS);
                consumed += CHANNELS;
                self.stats.drift_adjusted += 1;
            }
            let dst = &mut out[written * CHANNELS..(written + 1) * CHANNELS];
            if adjust && mode < 0 {
                // Falta: repetir la trama sin consumirla.
                for (d, v) in dst.iter_mut().zip(self.buf.iter()) {
                    *d = *v;
                }
                self.stats.drift_adjusted += 1;
            } else {
                for (d, v) in dst.iter_mut().zip(self.buf.drain(..CHANNELS)) {
                    *d = v;
                }
                consumed += CHANNELS;
            }
            written += 1;
        }
        let n = written * CHANNELS;
        if n < out.len() {
            out[n..].fill(0.0);
            self.stats.underruns += 1;
            self.priming = true;
            self.target_ms = (self.target_ms + STEP_MS).min(MAX_TARGET_MS);
            self.calm_values = 0;
        } else {
            self.calm_values += consumed as u64;
            let calm = ms_to_values(CALM_SECS * 1000) as u64;
            if self.calm_values >= calm && self.target_ms > TARGET_MS {
                self.target_ms = (self.target_ms - 10).max(TARGET_MS);
                self.calm_values = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Encoder;
    use crate::FRAME_LEN;

    fn packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = Encoder::new().unwrap();
        (0..n)
            .map(|i| {
                let f: Vec<f32> = (0..FRAME_LEN)
                    .map(|k| 0.2 * ((i * FRAME_LEN + k) as f32 * 0.03).sin())
                    .collect();
                enc.encode(&f).unwrap().to_vec()
            })
            .collect()
    }

    #[test]
    fn primes_before_playing_then_plays() {
        let p = packets(10);
        let mut j = JitterBuffer::new().unwrap();
        let mut out = vec![1.0; 960];
        // 3 tramas = 30 ms < 60 ms: aún silencio.
        for (i, pk) in p.iter().enumerate().take(3) {
            j.push(i as u32, pk);
        }
        j.pull(&mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        for (i, pk) in p.iter().enumerate().skip(3) {
            j.push(i as u32, pk);
        }
        j.pull(&mut out);
        assert!(out.iter().any(|v| *v != 0.0));
        assert_eq!(j.stats().packets, 10);
    }

    #[test]
    fn conceals_gaps_and_drops_late_packets() {
        let p = packets(10);
        let mut j = JitterBuffer::new().unwrap();
        j.push(0, &p[0]);
        j.push(3, &p[3]); // faltan 1 y 2
        assert_eq!(j.stats().concealed, 2);
        assert_eq!(j.buffered_values(), 4 * FRAME_LEN);
        j.push(1, &p[1]); // llegó tarde
        j.push(3, &p[3]); // repetido
        assert_eq!(j.stats().late_or_duplicate, 2);
    }

    #[test]
    fn underrun_and_trim() {
        let p = packets(40);
        let mut j = JitterBuffer::new().unwrap();
        for (i, pk) in p.iter().enumerate() {
            j.push(i as u32, pk);
        }
        // 400 ms entraron, el tope es 200 ms: se recortó al objetivo.
        assert!(j.buffered_values() <= ms_to_values(MAX_MS));
        assert!(j.stats().trimmed_values > 0);
        let mut big = vec![0.0; j.buffered_values() + 100];
        j.pull(&mut big);
        assert_eq!(j.stats().underruns, 1);
        // Tras vaciarse, vuelve a acumular antes de sonar.
        let mut out = vec![1.0; 100];
        j.push(40, &p[0]);
        j.pull(&mut out);
        assert!(out.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn target_grows_on_underrun_and_relaxes_when_calm() {
        let p = packets(1);
        let mut j = JitterBuffer::new().unwrap();
        assert_eq!(j.target_ms(), TARGET_MS);
        // Llenar justo el objetivo y pedir más: vaciado.
        for i in 0..6u32 {
            j.push(i, &p[0]);
        }
        let mut big = vec![0.0; j.buffered_values() + 2];
        j.pull(&mut big);
        assert_eq!(j.target_ms(), TARGET_MS + STEP_MS);
        // 10 s de reproducción sin vaciarse: baja un paso.
        let mut seq = 6u32;
        let mut out = vec![0.0; FRAME_LEN];
        for _ in 0..(CALM_SECS * 100 + 20) {
            j.push(seq, &p[0]);
            seq += 1;
            if j.buffered_values() >= ms_to_values(j.target_ms()) + FRAME_LEN {
                j.pull(&mut out);
            }
            j.pull(&mut out);
            j.push(seq, &p[0]);
            seq += 1;
        }
        assert!(j.target_ms() < TARGET_MS + STEP_MS, "{}", j.target_ms());
    }

    /// Emisor con el reloj un 0,3 % más lento o más rápido durante 30 s: sin
    /// compensar se vaciaría o recortaría; compensando, ni lo uno ni lo otro.
    #[test]
    fn compensates_clock_drift() {
        let p = packets(1);
        for rate in [0.997f64, 1.003] {
            let mut j = JitterBuffer::new().unwrap();
            let mut out = vec![0.0; FRAME_LEN];
            let (mut seq, mut credit) = (0u32, 0.0f64);
            // Cebado: llenar el objetivo.
            for _ in 0..7 {
                j.push(seq, &p[0]);
                seq += 1;
            }
            for _ in 0..3000 {
                credit += rate;
                while credit >= 1.0 {
                    j.push(seq, &p[0]);
                    seq += 1;
                    credit -= 1.0;
                }
                j.pull(&mut out);
            }
            let st = j.stats();
            assert_eq!(st.underruns, 0, "ritmo {rate}: {st:?}");
            assert_eq!(st.trimmed_values, 0, "ritmo {rate}: {st:?}");
            assert!(st.drift_adjusted > 0, "ritmo {rate}: {st:?}");
        }
    }

    #[test]
    fn sequence_wraps_around() {
        let p = packets(3);
        let mut j = JitterBuffer::new().unwrap();
        j.push(u32::MAX, &p[0]);
        j.push(0, &p[1]);
        j.push(1, &p[2]);
        assert_eq!(j.stats().late_or_duplicate, 0);
        assert_eq!(j.stats().concealed, 0);
    }
}
