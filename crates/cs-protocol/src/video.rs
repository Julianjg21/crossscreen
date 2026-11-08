//! Cabecera de los fragmentos de vídeo (un fragmento = un datagrama QUIC).
//!
//! Un frame codificado (unidad de acceso H.264 en Annex-B) se parte en
//! *shards* de igual tamaño. Los shards se agrupan en bloques FEC de hasta
//! [`MAX_DATA_SHARDS_PER_BLOCK`] shards de datos más sus shards de paridad
//! Reed-Solomon.
//!
//! ```text
//!  0       1       2       3       4       5       6       7
//! +-------+-------+-------+-------+-------+-------+-------+-------+
//! |  ver  | flags |   stream_id   |           frame_seq           |
//! +-------+-------+-------+-------+-------+-------+-------+-------+
//! |           frame_len           |            pts_us             |
//! +-------+-------+-------+-------+-------+-------+-------+-------+
//! | block | blocks| shard | data  | parity|  rsv  |   shard_len   |
//! +-------+-------+-------+-------+-------+-------+-------+-------+
//! ```
//! Todos los enteros en little-endian. `pts_us` es relativo al inicio de la
//! sesión y da la vuelta cada ~71 minutos (se compara con aritmética modular).

use crate::control::StreamId;

pub const HEADER_LEN: usize = 24;
pub const WIRE_VERSION: u8 = 1;
pub const MAX_DATA_SHARDS_PER_BLOCK: u8 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FragmentFlags(pub u8);

impl FragmentFlags {
    pub const KEYFRAME: u8 = 1 << 0;
    /// Este shard es de paridad FEC, no de datos.
    pub const PARITY: u8 = 1 << 1;
    /// El frame puede descartarse sin romper la cadena de referencias.
    pub const DISCARDABLE: u8 = 1 << 2;
    const KNOWN: u8 = Self::KEYFRAME | Self::PARITY | Self::DISCARDABLE;

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
    pub fn with(self, bit: u8) -> Self {
        FragmentFlags(self.0 | bit)
    }
    pub fn without(self, bit: u8) -> Self {
        FragmentFlags(self.0 & !bit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    pub flags: FragmentFlags,
    pub stream_id: StreamId,
    pub frame_seq: u32,
    /// Longitud total del frame sin relleno.
    pub frame_len: u32,
    pub pts_us: u32,
    pub block_idx: u8,
    pub block_count: u8,
    /// Índice dentro del bloque: `0..data_shards` datos, `data_shards..` paridad.
    pub shard_idx: u8,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub shard_len: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HeaderError {
    #[error("datagrama demasiado corto")]
    Truncated,
    #[error("versión de cabecera desconocida: {0}")]
    Version(u8),
    #[error("cabecera incoherente: {0}")]
    Invalid(&'static str),
}

impl FragmentHeader {
    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) {
        out[0] = WIRE_VERSION;
        out[1] = self.flags.0;
        out[2..4].copy_from_slice(&self.stream_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.frame_seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.pts_us.to_le_bytes());
        out[16] = self.block_idx;
        out[17] = self.block_count;
        out[18] = self.shard_idx;
        out[19] = self.data_shards;
        out[20] = self.parity_shards;
        out[21] = 0;
        out[22..24].copy_from_slice(&self.shard_len.to_le_bytes());
    }

    /// Decodifica y valida la cabecera; devuelve también la carga útil.
    pub fn decode(datagram: &[u8]) -> Result<(FragmentHeader, &[u8]), HeaderError> {
        if datagram.len() < HEADER_LEN {
            return Err(HeaderError::Truncated);
        }
        let b = datagram;
        if b[0] != WIRE_VERSION {
            return Err(HeaderError::Version(b[0]));
        }
        let le16 = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let le32 = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let h = FragmentHeader {
            flags: FragmentFlags(b[1]),
            stream_id: le16(2),
            frame_seq: le32(4),
            frame_len: le32(8),
            pts_us: le32(12),
            block_idx: b[16],
            block_count: b[17],
            shard_idx: b[18],
            data_shards: b[19],
            parity_shards: b[20],
            shard_len: le16(22),
        };
        h.validate()?;
        let payload = &datagram[HEADER_LEN..];
        if payload.len() != usize::from(h.shard_len) {
            return Err(HeaderError::Invalid(
                "longitud de carga distinta de shard_len",
            ));
        }
        Ok((h, payload))
    }

    fn validate(&self) -> Result<(), HeaderError> {
        use HeaderError::Invalid;
        if self.flags.0 & !FragmentFlags::KNOWN != 0 {
            return Err(Invalid("flags desconocidos"));
        }
        if self.block_count == 0 || self.block_idx >= self.block_count {
            return Err(Invalid("índice de bloque"));
        }
        if self.data_shards == 0 || self.data_shards > MAX_DATA_SHARDS_PER_BLOCK {
            return Err(Invalid("data_shards"));
        }
        if u16::from(self.shard_idx) >= u16::from(self.data_shards) + u16::from(self.parity_shards)
        {
            return Err(Invalid("shard_idx"));
        }
        let is_parity = self.shard_idx >= self.data_shards;
        if is_parity != self.flags.has(FragmentFlags::PARITY) {
            return Err(Invalid("flag de paridad incoherente"));
        }
        if self.shard_len == 0 || self.shard_len % 2 != 0 {
            return Err(Invalid("shard_len debe ser par y no nulo"));
        }
        if self.frame_len == 0 {
            return Err(Invalid("frame vacío"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample() -> FragmentHeader {
        FragmentHeader {
            flags: FragmentFlags(FragmentFlags::KEYFRAME),
            stream_id: 1,
            frame_seq: 0xDEAD_BEEF,
            frame_len: 5000,
            pts_us: 123_456,
            block_idx: 0,
            block_count: 1,
            shard_idx: 2,
            data_shards: 5,
            parity_shards: 1,
            shard_len: 1000,
        }
    }

    fn datagram(h: &FragmentHeader) -> Vec<u8> {
        let mut hb = [0u8; HEADER_LEN];
        h.encode(&mut hb);
        let mut v = hb.to_vec();
        v.resize(HEADER_LEN + usize::from(h.shard_len), 0xAB);
        v
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let d = datagram(&h);
        let (h2, payload) = FragmentHeader::decode(&d).unwrap();
        assert_eq!(h, h2);
        assert_eq!(payload.len(), 1000);
    }

    #[test]
    fn rejects_inconsistent() {
        let mut h = sample();
        h.shard_idx = 5; // sería paridad pero no lleva el flag
        assert!(FragmentHeader::decode(&datagram(&h)).is_err());
        let mut h = sample();
        h.block_idx = 1;
        assert!(FragmentHeader::decode(&datagram(&h)).is_err());
        let mut d = datagram(&sample());
        d[0] = 99;
        assert_eq!(FragmentHeader::decode(&d), Err(HeaderError::Version(99)));
        assert_eq!(
            FragmentHeader::decode(&d[..10]),
            Err(HeaderError::Truncated)
        );
    }

    proptest! {
        #[test]
        fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let _ = FragmentHeader::decode(&bytes);
        }
    }
}
