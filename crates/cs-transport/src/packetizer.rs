//! Frame codificado → datagramas (con FEC Reed-Solomon por bloque).

use cs_protocol::control::StreamId;
use cs_protocol::video::{FragmentFlags, FragmentHeader, HEADER_LEN, MAX_DATA_SHARDS_PER_BLOCK};

/// Tamaño máximo de frame aceptado (un IDR 4K a bitrate alto ronda 1-2 MB).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
const MAX_SHARDS_PER_BLOCK: usize = 255;
const MIN_PAYLOAD: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacketizerConfig {
    /// Tamaño máximo de datagrama (cabecera incluida); normalmente
    /// `Connection::max_datagram_size()`.
    pub max_datagram: usize,
    /// Proporción de shards de paridad respecto a los de datos (0.0 = sin FEC).
    pub fec_ratio: f32,
}

impl Default for PacketizerConfig {
    fn default() -> Self {
        PacketizerConfig {
            max_datagram: 1200,
            fec_ratio: 0.10,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PacketizeError {
    #[error("frame vacío")]
    Empty,
    #[error("frame demasiado grande: {0} bytes")]
    TooLarge(usize),
    #[error("max_datagram demasiado pequeño")]
    DatagramTooSmall,
    #[error("FEC: {0}")]
    Fec(String),
}

/// Metadatos por frame que acompañan al bitstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_seq: u32,
    pub pts_us: u32,
    pub keyframe: bool,
    pub discardable: bool,
}

#[derive(Debug, Clone)]
pub struct Packetizer {
    stream_id: StreamId,
    cfg: PacketizerConfig,
}

impl Packetizer {
    pub fn new(stream_id: StreamId, cfg: PacketizerConfig) -> Self {
        Packetizer { stream_id, cfg }
    }

    pub fn config(&self) -> PacketizerConfig {
        self.cfg
    }

    pub fn set_fec_ratio(&mut self, ratio: f32) {
        self.cfg.fec_ratio = ratio.clamp(0.0, 1.0);
    }

    pub fn set_max_datagram(&mut self, max: usize) {
        self.cfg.max_datagram = max;
    }

    /// Parte `frame` en datagramas listos para enviar, en orden de envío
    /// (por bloque: datos y luego paridad).
    pub fn packetize(&self, meta: FrameMeta, frame: &[u8]) -> Result<Vec<Vec<u8>>, PacketizeError> {
        if frame.is_empty() {
            return Err(PacketizeError::Empty);
        }
        if frame.len() > MAX_FRAME_LEN {
            return Err(PacketizeError::TooLarge(frame.len()));
        }
        let max_payload = self
            .cfg
            .max_datagram
            .saturating_sub(HEADER_LEN)
            .min(usize::from(u16::MAX))
            & !1;
        if max_payload < MIN_PAYLOAD {
            return Err(PacketizeError::DatagramTooSmall);
        }

        let total_data = frame.len().div_ceil(max_payload);
        let block_count = total_data.div_ceil(usize::from(MAX_DATA_SHARDS_PER_BLOCK));
        if block_count > usize::from(u8::MAX) {
            return Err(PacketizeError::TooLarge(frame.len()));
        }
        // Repartir los bytes de forma uniforme entre shards (minimiza relleno).
        let shard_len = round_up_even(frame.len().div_ceil(total_data));
        debug_assert!(shard_len <= max_payload);

        let mut flags = FragmentFlags::default();
        if meta.keyframe {
            flags = flags.with(FragmentFlags::KEYFRAME);
        }
        if meta.discardable {
            flags = flags.with(FragmentFlags::DISCARDABLE);
        }

        let base = total_data / block_count;
        let extra = total_data % block_count;
        let mut out = Vec::new();
        let mut global_shard = 0usize;

        for block_idx in 0..block_count {
            let data_shards = base + usize::from(block_idx < extra);
            let parity_shards = self.parity_for(data_shards);

            let shards: Vec<Vec<u8>> = (0..data_shards)
                .map(|i| {
                    let start = (global_shard + i) * shard_len;
                    let mut s = vec![0u8; shard_len];
                    if start < frame.len() {
                        let end = (start + shard_len).min(frame.len());
                        s[..end - start].copy_from_slice(&frame[start..end]);
                    }
                    s
                })
                .collect();
            global_shard += data_shards;

            let parity = if parity_shards > 0 {
                reed_solomon_simd::encode(data_shards, parity_shards, &shards)
                    .map_err(|e| PacketizeError::Fec(e.to_string()))?
            } else {
                Vec::new()
            };

            let header = |shard_idx: usize, is_parity: bool| FragmentHeader {
                flags: if is_parity {
                    flags.with(FragmentFlags::PARITY)
                } else {
                    flags
                },
                stream_id: self.stream_id,
                frame_seq: meta.frame_seq,
                frame_len: frame.len() as u32,
                pts_us: meta.pts_us,
                block_idx: block_idx as u8,
                block_count: block_count as u8,
                shard_idx: shard_idx as u8,
                data_shards: data_shards as u8,
                parity_shards: parity_shards as u8,
                shard_len: shard_len as u16,
            };

            for (i, s) in shards.iter().enumerate() {
                out.push(datagram(&header(i, false), s));
            }
            for (j, p) in parity.iter().enumerate() {
                out.push(datagram(&header(data_shards + j, true), p));
            }
        }
        Ok(out)
    }

    fn parity_for(&self, data_shards: usize) -> usize {
        if self.cfg.fec_ratio <= 0.0 {
            return 0;
        }
        let p = ((data_shards as f32) * self.cfg.fec_ratio).ceil() as usize;
        p.max(1).min(MAX_SHARDS_PER_BLOCK - data_shards)
    }
}

fn round_up_even(n: usize) -> usize {
    (n + 1) & !1
}

fn datagram(h: &FragmentHeader, payload: &[u8]) -> Vec<u8> {
    let mut hb = [0u8; HEADER_LEN];
    h.encode(&mut hb);
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    v.extend_from_slice(&hb);
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(seq: u32) -> FrameMeta {
        FrameMeta {
            frame_seq: seq,
            pts_us: seq * 16_666,
            keyframe: seq == 0,
            discardable: false,
        }
    }

    #[test]
    fn datagrams_respect_max_size() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.2,
            },
        );
        let frame = vec![7u8; 100_000];
        let dgs = p.packetize(meta(0), &frame).unwrap();
        assert!(dgs.iter().all(|d| d.len() <= 1200));
        // 100_000 / 1176 → 86 shards de datos, 2 bloques (43+43), 9 de paridad cada uno.
        assert_eq!(dgs.len(), 86 + 18);
    }

    #[test]
    fn small_frame_single_datagram_plus_parity() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.1,
            },
        );
        let dgs = p.packetize(meta(1), b"hola").unwrap();
        assert_eq!(dgs.len(), 2);
        assert_eq!(dgs[0].len(), HEADER_LEN + 4);
    }

    #[test]
    fn no_fec() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.0,
            },
        );
        assert_eq!(p.packetize(meta(1), &[1u8; 5000]).unwrap().len(), 5);
    }

    #[test]
    fn errors() {
        let p = Packetizer::new(1, PacketizerConfig::default());
        assert_eq!(p.packetize(meta(1), &[]), Err(PacketizeError::Empty));
        let tiny = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 40,
                fec_ratio: 0.0,
            },
        );
        assert_eq!(
            tiny.packetize(meta(1), &[1]),
            Err(PacketizeError::DatagramTooSmall)
        );
    }
}
