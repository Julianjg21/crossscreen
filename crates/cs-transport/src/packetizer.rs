//! Frame codificado → datagramas (con FEC Reed-Solomon por bloque).
//!
//! ## Resiliencia ante pérdidas en ráfaga
//!
//! El formato del cable no cambia (cada cabecera lleva los shards de datos y
//! de paridad de su bloque), así que el reensamblador no necesita saber nada
//! de estas opciones:
//!
//! - **FEC distinto para IDR** ([`Packetizer::with_keyframe_fec_ratio`]): un
//!   IDR es 2–3 veces mayor que un P y perderlo obliga a pedir otro, así que
//!   se protege más (por defecto del emisor, 0,5).
//! - **Entrelazado** ([`Packetizer::with_interleave`]): los shards de un frame
//!   con varios bloques se envían por turnos (bloque 0, 1, …, 0, 1, …) en vez
//!   de bloque a bloque, para que una ráfaga de datagramas consecutivos se
//!   reparta entre los bloques en lugar de agotar la paridad de uno solo.
//! - **Tamaño máximo de bloque** ([`Packetizer::with_max_block_data_shards`]):
//!   bloques más pequeños solo ayudan si van entrelazados, y aun así un único
//!   bloque Reed-Solomon (MDS) recupera *cualquier* combinación de pérdidas
//!   hasta su paridad, así que por defecto se usa el máximo del cable (64).

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
    /// Paridad de los IDR (se usa el mayor de este valor y `cfg.fec_ratio`).
    keyframe_fec_ratio: f32,
    interleave: bool,
    max_block_data: usize,
}

impl Packetizer {
    /// Comportamiento clásico: misma paridad para todos los frames, bloques
    /// de hasta 64 shards de datos y envío bloque a bloque.
    pub fn new(stream_id: StreamId, cfg: PacketizerConfig) -> Self {
        Packetizer {
            stream_id,
            keyframe_fec_ratio: cfg.fec_ratio,
            cfg,
            interleave: false,
            max_block_data: usize::from(MAX_DATA_SHARDS_PER_BLOCK),
        }
    }

    /// Paridad propia de los IDR (nunca menor que la de los P).
    pub fn with_keyframe_fec_ratio(mut self, ratio: f32) -> Self {
        self.set_keyframe_fec_ratio(ratio);
        self
    }

    /// Envía los shards de un frame de varios bloques por turnos.
    pub fn with_interleave(mut self, on: bool) -> Self {
        self.interleave = on;
        self
    }

    /// Máximo de shards de datos por bloque (1..=64, el límite del cable).
    pub fn with_max_block_data_shards(mut self, n: usize) -> Self {
        self.max_block_data = n.clamp(1, usize::from(MAX_DATA_SHARDS_PER_BLOCK));
        self
    }

    pub fn config(&self) -> PacketizerConfig {
        self.cfg
    }

    pub fn keyframe_fec_ratio(&self) -> f32 {
        self.keyframe_fec_ratio
    }

    pub fn interleave(&self) -> bool {
        self.interleave
    }

    /// Paridad de los frames no clave.
    pub fn set_fec_ratio(&mut self, ratio: f32) {
        self.cfg.fec_ratio = ratio.clamp(0.0, 1.0);
    }

    pub fn set_keyframe_fec_ratio(&mut self, ratio: f32) {
        self.keyframe_fec_ratio = ratio.clamp(0.0, 1.0);
    }

    pub fn set_max_datagram(&mut self, max: usize) {
        self.cfg.max_datagram = max;
    }

    /// Parte `frame` en datagramas listos para enviar, en orden de envío.
    /// Sin entrelazado: por bloque, datos y luego paridad. Con entrelazado:
    /// el shard `i` de cada bloque, luego el `i + 1` de cada bloque, etc.
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
        let block_count = total_data.div_ceil(self.max_block_data);
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

        let ratio = if meta.keyframe {
            self.keyframe_fec_ratio.max(self.cfg.fec_ratio)
        } else {
            self.cfg.fec_ratio
        };
        let base = total_data / block_count;
        let extra = total_data % block_count;
        // Datagramas de cada bloque, en orden (datos y luego paridad).
        let mut blocks: Vec<Vec<Vec<u8>>> = Vec::with_capacity(block_count);
        let mut global_shard = 0usize;

        for block_idx in 0..block_count {
            let data_shards = base + usize::from(block_idx < extra);
            let parity_shards = parity_for(data_shards, ratio);

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

            let mut dgs = Vec::with_capacity(data_shards + parity.len());
            for (i, s) in shards.iter().enumerate() {
                dgs.push(datagram(&header(i, false), s));
            }
            for (j, p) in parity.iter().enumerate() {
                dgs.push(datagram(&header(data_shards + j, true), p));
            }
            blocks.push(dgs);
        }
        if !self.interleave || blocks.len() == 1 {
            return Ok(blocks.into_iter().flatten().collect());
        }
        Ok(interleave(blocks))
    }
}

/// Shards de paridad para un bloque de `data_shards` con proporción `ratio`
/// (al menos 1 si hay FEC).
fn parity_for(data_shards: usize, ratio: f32) -> usize {
    if ratio <= 0.0 {
        return 0;
    }
    let p = ((data_shards as f32) * ratio).ceil() as usize;
    p.max(1).min(MAX_SHARDS_PER_BLOCK - data_shards)
}

/// Orden por turnos: el elemento `i` de cada bloque, luego el `i + 1`…
fn interleave(blocks: Vec<Vec<Vec<u8>>>) -> Vec<Vec<u8>> {
    let total = blocks.iter().map(Vec::len).sum();
    let mut iters: Vec<_> = blocks.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for it in &mut iters {
            if let Some(d) = it.next() {
                out.push(d);
            }
        }
    }
    out
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

    fn hdr(d: &[u8]) -> FragmentHeader {
        FragmentHeader::decode(d).unwrap().0
    }

    fn cfg(fec: f32) -> PacketizerConfig {
        PacketizerConfig {
            max_datagram: 1200,
            fec_ratio: fec,
        }
    }

    #[test]
    fn keyframes_get_their_own_parity() {
        let p = Packetizer::new(1, cfg(0.1)).with_keyframe_fec_ratio(0.5);
        let frame = vec![3u8; 100_000]; // 86 shards de datos: 2 bloques de 43
        let key = p.packetize(meta(0), &frame).unwrap();
        let delta = p.packetize(meta(1), &frame).unwrap();
        // IDR: ceil(43·0,5) = 22 de paridad por bloque; P: ceil(4,3) = 5.
        assert_eq!(key.len(), 86 + 2 * 22);
        assert_eq!(delta.len(), 86 + 2 * 5);
        assert!(key.iter().all(|d| hdr(d).parity_shards == 22));
        // La paridad de los IDR nunca baja de la de los P.
        let p = Packetizer::new(1, cfg(0.3)).with_keyframe_fec_ratio(0.1);
        assert_eq!(p.packetize(meta(0), &frame).unwrap().len(), 86 + 2 * 13);
    }

    #[test]
    fn interleave_round_robin_across_blocks() {
        let p = Packetizer::new(1, cfg(0.1)).with_interleave(true);
        let frame = vec![9u8; 100_000];
        let dgs = p.packetize(meta(1), &frame).unwrap();
        assert_eq!(dgs.len(), 86 + 10);
        let order: Vec<(u8, u8)> = dgs
            .iter()
            .map(|d| (hdr(d).block_idx, hdr(d).shard_idx))
            .collect();
        // Bloques alternos: (0,0) (1,0) (0,1) (1,1) …
        for (i, (b, s)) in order.iter().enumerate() {
            assert_eq!(usize::from(*b), i % 2, "datagrama {i}");
            assert_eq!(usize::from(*s), i / 2, "datagrama {i}");
        }
        // Mismo conjunto de datagramas que sin entrelazar.
        let mut a = dgs.clone();
        let mut b = Packetizer::new(1, cfg(0.1))
            .packetize(meta(1), &frame)
            .unwrap();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn interleave_uneven_blocks_and_small_blocks() {
        // 5 shards de datos en bloques de 2 → bloques de 2, 2 y 1 (+1 paridad).
        let p = Packetizer::new(1, cfg(0.1))
            .with_interleave(true)
            .with_max_block_data_shards(2);
        let dgs = p.packetize(meta(1), &[1u8; 5 * 1176]).unwrap();
        let order: Vec<(u8, u8)> = dgs
            .iter()
            .map(|d| (hdr(d).block_idx, hdr(d).shard_idx))
            .collect();
        assert_eq!(
            order,
            vec![
                (0, 0),
                (1, 0),
                (2, 0),
                (0, 1),
                (1, 1),
                (2, 1),
                (0, 2),
                (1, 2)
            ]
        );
        assert!(dgs.iter().all(|d| hdr(d).block_count == 3));
    }

    #[test]
    fn interleave_single_block_is_unchanged() {
        let a = Packetizer::new(1, cfg(0.1)).with_interleave(true);
        let b = Packetizer::new(1, cfg(0.1));
        let f = vec![5u8; 30_000];
        assert_eq!(
            a.packetize(meta(1), &f).unwrap(),
            b.packetize(meta(1), &f).unwrap()
        );
    }
}
