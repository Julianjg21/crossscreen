//! Datagramas → frames completos, recuperando pérdidas con FEC.
//!
//! Política de entrega (baja latencia): los frames se entregan en orden.
//! Cuando un frame `N` se completa, todo frame pendiente `< N` se declara
//! perdido (los fragmentos se envían en orden, así que si `N` ya está entero
//! los de frames anteriores que faltan casi seguro se han perdido). Los
//! fragmentos que lleguen después para frames ya entregados o perdidos se
//! descartan. No hay retransmisión de vídeo: la recuperación es FEC + IDR o
//! invalidación de referencias a nivel de sesión.

use crate::packetizer::MAX_FRAME_LEN;
use cs_protocol::control::StreamId;
use cs_protocol::video::{FragmentFlags, FragmentHeader, HeaderError};
use std::collections::BTreeMap;

/// Máximo de frames incompletos retenidos; el más antiguo se descarta.
const MAX_PENDING_FRAMES: usize = 16;
/// Máximo de frames de un hueco de `frame_seq` que se notifican como perdidos.
const MAX_GAP_REPORT: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteFrame {
    pub stream_id: StreamId,
    pub frame_seq: u32,
    pub pts_us: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Shards de datos reconstruidos mediante FEC en este frame.
    pub recovered_shards: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyEvent {
    Frame(CompleteFrame),
    Lost { frame_seq: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub datagrams: u64,
    pub frames_delivered: u64,
    pub frames_lost: u64,
    pub shards_recovered: u64,
    /// Fragmentos de frames ya declarados perdidos (llegaron demasiado tarde).
    pub late_datagrams: u64,
    /// Fragmentos sobrantes de frames ya entregados (normalmente paridad que
    /// llega después de reconstruir el bloque). No indican ningún problema.
    pub redundant_datagrams: u64,
    /// Fragmentos con cabecera inválida o incoherente con el resto del frame.
    pub invalid_datagrams: u64,
    pub duplicate_datagrams: u64,
}

#[derive(Debug)]
struct Block {
    data_shards: usize,
    parity_shards: usize,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    /// Shards de datos (completos o reconstruidos) cuando el bloque está listo.
    done: Option<Vec<Vec<u8>>>,
}

#[derive(Debug)]
struct PendingFrame {
    frame_len: u32,
    pts_us: u32,
    keyframe: bool,
    shard_len: u16,
    blocks: Vec<Option<Block>>,
    blocks_done: usize,
    recovered: u32,
}

#[derive(Debug)]
pub struct Reassembler {
    stream_id: StreamId,
    pending: BTreeMap<u32, PendingFrame>,
    /// Último frame entregado o declarado perdido; todo lo `<=` es tardío.
    horizon: Option<u32>,
    /// Frames entregados recientemente (para clasificar shards sobrantes).
    recently_delivered: std::collections::VecDeque<u32>,
    stats: ReassemblyStats,
}

impl Reassembler {
    pub fn new(stream_id: StreamId) -> Self {
        Reassembler {
            stream_id,
            pending: BTreeMap::new(),
            horizon: None,
            recently_delivered: std::collections::VecDeque::with_capacity(MAX_PENDING_FRAMES),
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// Procesa un datagrama. Los errores de cabecera se cuentan y se
    /// devuelven para registro, pero no invalidan el estado.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Vec<ReassemblyEvent>, HeaderError> {
        self.stats.datagrams += 1;
        let (h, payload) = match FragmentHeader::decode(datagram) {
            Ok(v) => v,
            Err(e) => {
                self.stats.invalid_datagrams += 1;
                return Err(e);
            }
        };
        if h.stream_id != self.stream_id || h.frame_len as usize > MAX_FRAME_LEN {
            self.stats.invalid_datagrams += 1;
            return Ok(Vec::new());
        }
        if self.horizon.is_some_and(|hz| h.frame_seq <= hz) {
            if self.recently_delivered.contains(&h.frame_seq) {
                self.stats.redundant_datagrams += 1;
            } else {
                self.stats.late_datagrams += 1;
            }
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        if !self.pending.contains_key(&h.frame_seq) && self.pending.len() >= MAX_PENDING_FRAMES {
            if let Some((&oldest, _)) = self.pending.iter().next() {
                if oldest < h.frame_seq {
                    self.pending.remove(&oldest);
                    self.mark_lost(oldest, &mut events);
                } else {
                    // Fragmento más antiguo que todo lo retenido: no cabe.
                    self.stats.late_datagrams += 1;
                    return Ok(events);
                }
            }
        }

        let frame = self
            .pending
            .entry(h.frame_seq)
            .or_insert_with(|| PendingFrame {
                frame_len: h.frame_len,
                pts_us: h.pts_us,
                keyframe: h.flags.has(FragmentFlags::KEYFRAME),
                shard_len: h.shard_len,
                blocks: (0..h.block_count).map(|_| None).collect(),
                blocks_done: 0,
                recovered: 0,
            });

        if frame.frame_len != h.frame_len
            || frame.shard_len != h.shard_len
            || frame.blocks.len() != usize::from(h.block_count)
        {
            self.stats.invalid_datagrams += 1;
            return Ok(events);
        }

        let block = frame.blocks[usize::from(h.block_idx)].get_or_insert_with(|| Block {
            data_shards: usize::from(h.data_shards),
            parity_shards: usize::from(h.parity_shards),
            shards: vec![None; usize::from(h.data_shards) + usize::from(h.parity_shards)],
            received: 0,
            done: None,
        });
        if block.data_shards != usize::from(h.data_shards)
            || block.parity_shards != usize::from(h.parity_shards)
        {
            self.stats.invalid_datagrams += 1;
            return Ok(events);
        }
        if block.done.is_some() {
            // Bloque ya completo: los shards sobrantes (paridad) se ignoran.
            return Ok(events);
        }
        let slot = &mut block.shards[usize::from(h.shard_idx)];
        if slot.is_some() {
            self.stats.duplicate_datagrams += 1;
            return Ok(events);
        }
        *slot = Some(payload.to_vec());
        block.received += 1;

        if block.received >= block.data_shards {
            match complete_block(block) {
                Ok(recovered) => {
                    frame.recovered += recovered;
                    frame.blocks_done += 1;
                }
                Err(_) => {
                    self.stats.invalid_datagrams += 1;
                    return Ok(events);
                }
            }
        }

        if frame.blocks_done == frame.blocks.len() {
            let seq = h.frame_seq;
            let frame = self.pending.remove(&seq).expect("frame pendiente");
            // Perdidos: los pendientes anteriores (llegó algo pero no bastó) y
            // también los huecos de `frame_seq` de los que no llegó **ningún**
            // datagrama (habitual en P-frames de 1-2 datagramas; detectado en
            // la prueba de extremo a extremo del MVP).
            let mut lost: std::collections::BTreeSet<u32> =
                self.pending.range(..seq).map(|(k, _)| *k).collect();
            for s in &lost {
                self.pending.remove(s);
            }
            if let Some(prev) = self.horizon {
                let gap_start = prev.saturating_add(1);
                // Acotado: un salto enorme (p. ej. reinicio del emisor) no debe
                // generar millones de eventos.
                let gap_end = seq.min(gap_start.saturating_add(MAX_GAP_REPORT));
                lost.extend(gap_start..gap_end);
            }
            for s in lost {
                self.mark_lost(s, &mut events);
            }
            self.horizon = Some(seq);
            if self.recently_delivered.len() == MAX_PENDING_FRAMES {
                self.recently_delivered.pop_front();
            }
            self.recently_delivered.push_back(seq);
            self.stats.frames_delivered += 1;
            self.stats.shards_recovered += u64::from(frame.recovered);
            events.push(ReassemblyEvent::Frame(assemble(self.stream_id, seq, frame)));
        }
        Ok(events)
    }

    /// Declara perdidos todos los frames pendientes (p. ej. tras un periodo sin
    /// datos, o antes de pedir un IDR al reanudar).
    pub fn abandon_pending(&mut self) -> Vec<ReassemblyEvent> {
        let mut events = Vec::new();
        let seqs: Vec<u32> = self.pending.keys().copied().collect();
        self.pending.clear();
        for s in seqs {
            self.mark_lost(s, &mut events);
        }
        events
    }

    fn mark_lost(&mut self, seq: u32, events: &mut Vec<ReassemblyEvent>) {
        self.stats.frames_lost += 1;
        self.horizon = Some(self.horizon.map_or(seq, |h| h.max(seq)));
        events.push(ReassemblyEvent::Lost { frame_seq: seq });
    }
}

/// Completa un bloque con al menos `data_shards` shards. Devuelve cuántos
/// shards de datos se reconstruyeron con FEC.
fn complete_block(block: &mut Block) -> Result<u32, reed_solomon_simd::Error> {
    let k = block.data_shards;
    let missing: Vec<usize> = (0..k).filter(|i| block.shards[*i].is_none()).collect();
    let recovered = missing.len() as u32;
    if !missing.is_empty() {
        let originals = (0..k).filter_map(|i| block.shards[i].as_ref().map(|s| (i, s)));
        let recovery =
            (0..block.parity_shards).filter_map(|j| block.shards[k + j].as_ref().map(|s| (j, s)));
        let restored = reed_solomon_simd::decode(k, block.parity_shards, originals, recovery)?;
        for (i, data) in restored {
            block.shards[i] = Some(data);
        }
    }
    let data: Vec<Vec<u8>> = block
        .shards
        .drain(..k)
        .map(|s| s.expect("shard de datos"))
        .collect();
    block.shards.clear();
    block.done = Some(data);
    Ok(recovered)
}

fn assemble(stream_id: StreamId, frame_seq: u32, frame: PendingFrame) -> CompleteFrame {
    let mut data = Vec::with_capacity(frame.frame_len as usize + usize::from(frame.shard_len));
    for block in frame.blocks.into_iter().flatten() {
        for shard in block.done.into_iter().flatten() {
            data.extend_from_slice(&shard);
        }
    }
    data.truncate(frame.frame_len as usize);
    CompleteFrame {
        stream_id,
        frame_seq,
        pts_us: frame.pts_us,
        keyframe: frame.keyframe,
        data,
        recovered_shards: frame.recovered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packetizer::{FrameMeta, Packetizer, PacketizerConfig};
    use proptest::prelude::*;
    use rand::{seq::SliceRandom, Rng, SeedableRng};

    fn meta(seq: u32) -> FrameMeta {
        FrameMeta {
            frame_seq: seq,
            pts_us: seq * 1000,
            keyframe: seq % 30 == 0,
            discardable: false,
        }
    }

    fn frame_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn only_frames(evs: Vec<ReassemblyEvent>) -> Vec<CompleteFrame> {
        evs.into_iter()
            .filter_map(|e| match e {
                ReassemblyEvent::Frame(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lossless_roundtrip() {
        let p = Packetizer::new(
            3,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.1,
            },
        );
        let mut r = Reassembler::new(3);
        for seq in 0..10u32 {
            let data = frame_bytes(1000 + seq as usize * 7000, seq as u8);
            let mut got = Vec::new();
            for d in p.packetize(meta(seq), &data).unwrap() {
                got.extend(only_frames(r.push(&d).unwrap()));
            }
            assert_eq!(got.len(), 1, "frame {seq}");
            assert_eq!(got[0].data, data);
            assert_eq!(got[0].keyframe, seq == 0);
            assert_eq!(got[0].recovered_shards, 0);
        }
        assert_eq!(r.stats().frames_delivered, 10);
    }

    #[test]
    fn recovers_losses_within_parity_any_order() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.25,
            },
        );
        let data = frame_bytes(200_000, 9); // varios bloques
        let mut dgs = p.packetize(meta(5), &data).unwrap();
        // Quitar un shard de datos por bloque (paridad ≥ 1 por bloque) y barajar.
        let mut seen_blocks = std::collections::HashSet::new();
        dgs.retain(|d| {
            let (h, _) = FragmentHeader::decode(d).unwrap();
            !(h.shard_idx < h.data_shards && seen_blocks.insert(h.block_idx))
        });
        dgs.shuffle(&mut rand::rngs::StdRng::seed_from_u64(1));
        let mut r = Reassembler::new(1);
        let mut frames = Vec::new();
        for d in &dgs {
            frames.extend(only_frames(r.push(d).unwrap()));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        // Con orden barajado la paridad puede llegar antes que algunos datos y
        // el bloque se reconstruye en cuanto hay `k` shards: se recuperan al
        // menos los eliminados (uno por bloque), posiblemente más.
        assert!(frames[0].recovered_shards as usize >= seen_blocks.len());
    }

    #[test]
    fn unrecoverable_frame_reported_lost_when_next_completes() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.0,
            },
        );
        let mut r = Reassembler::new(1);
        let f0 = p.packetize(meta(0), &frame_bytes(5000, 0)).unwrap();
        let f1 = p.packetize(meta(1), &frame_bytes(5000, 1)).unwrap();
        for d in &f0[1..] {
            assert!(r.push(d).unwrap().is_empty());
        }
        let mut evs = Vec::new();
        for d in &f1 {
            evs.extend(r.push(d).unwrap());
        }
        assert_eq!(evs[0], ReassemblyEvent::Lost { frame_seq: 0 });
        assert!(matches!(&evs[1], ReassemblyEvent::Frame(f) if f.frame_seq == 1));
        // El shard tardío del frame 0 se descarta.
        assert!(r.push(&f0[0]).unwrap().is_empty());
        assert_eq!(r.stats().late_datagrams, 1);
        assert_eq!(r.stats().frames_lost, 1);
    }

    #[test]
    fn duplicates_and_garbage_are_counted_not_fatal() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.0,
            },
        );
        let dgs = p.packetize(meta(0), &frame_bytes(3000, 0)).unwrap();
        let mut r = Reassembler::new(1);
        r.push(&dgs[0]).unwrap();
        r.push(&dgs[0]).unwrap();
        assert!(r.push(&[1, 2, 3]).is_err());
        assert_eq!(r.stats().duplicate_datagrams, 1);
        assert_eq!(r.stats().invalid_datagrams, 1);
    }

    #[test]
    fn frame_with_no_datagrams_at_all_is_reported_lost() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.0,
            },
        );
        let mut r = Reassembler::new(1);
        let mut evs = Vec::new();
        // Se envían 0, 1, (2 y 3 se pierden enteros), 4.
        for seq in [0u32, 1, 4] {
            for d in p
                .packetize(meta(seq), &frame_bytes(800, seq as u8))
                .unwrap()
            {
                evs.extend(r.push(&d).unwrap());
            }
        }
        let summary: Vec<String> = evs
            .iter()
            .map(|e| match e {
                ReassemblyEvent::Frame(f) => format!("F{}", f.frame_seq),
                ReassemblyEvent::Lost { frame_seq } => format!("L{frame_seq}"),
            })
            .collect();
        assert_eq!(summary, ["F0", "F1", "L2", "L3", "F4"]);
        assert_eq!(r.stats().frames_lost, 2);
    }

    #[test]
    fn pending_is_bounded() {
        let p = Packetizer::new(
            1,
            PacketizerConfig {
                max_datagram: 1200,
                fec_ratio: 0.0,
            },
        );
        let mut r = Reassembler::new(1);
        let mut lost = 0;
        for seq in 0..(MAX_PENDING_FRAMES as u32 + 5) {
            let dgs = p.packetize(meta(seq), &frame_bytes(3000, 0)).unwrap();
            for e in r.push(&dgs[0]).unwrap() {
                if matches!(e, ReassemblyEvent::Lost { .. }) {
                    lost += 1;
                }
            }
        }
        assert_eq!(lost, 5);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// Con pérdidas ≤ paridad por bloque, el frame siempre se reconstruye.
        #[test]
        fn fec_recovers_up_to_parity(len in 1usize..300_000, ratio in 0.05f32..0.5, seed in any::<u64>()) {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let p = Packetizer::new(1, PacketizerConfig { max_datagram: 1200, fec_ratio: ratio });
            let data: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
            let dgs = p.packetize(meta(1), &data).unwrap();
            // Agrupar por bloque y eliminar hasta `parity` shards al azar en cada uno.
            let mut by_block: BTreeMap<u8, Vec<Vec<u8>>> = BTreeMap::new();
            for d in dgs {
                let (h, _) = FragmentHeader::decode(&d).unwrap();
                by_block.entry(h.block_idx).or_default().push(d);
            }
            let mut kept = Vec::new();
            for (_, mut v) in by_block {
                let (h, _) = FragmentHeader::decode(&v[0]).unwrap();
                v.shuffle(&mut rng);
                let drop = rng.gen_range(0..=usize::from(h.parity_shards));
                kept.extend(v.into_iter().skip(drop));
            }
            kept.shuffle(&mut rng);
            let mut r = Reassembler::new(1);
            let mut frames = Vec::new();
            for d in &kept { frames.extend(only_frames(r.push(d).unwrap())); }
            prop_assert_eq!(frames.len(), 1);
            prop_assert_eq!(&frames[0].data, &data);
        }
    }
}
