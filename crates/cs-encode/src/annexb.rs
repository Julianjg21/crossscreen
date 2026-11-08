//! Flujo H.264 en formato Annex-B: separación de NAL por códigos de inicio.

/// Tipos de NAL relevantes (H.264 tabla 7-1).
pub mod nal_type {
    pub const NON_IDR_SLICE: u8 = 1;
    pub const IDR_SLICE: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// Una NAL dentro de un búfer Annex-B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal<'a> {
    /// Desplazamiento del código de inicio en el búfer original.
    pub offset: usize,
    /// Longitud del código de inicio (3 o 4 bytes).
    pub start_code_len: usize,
    /// Bytes de la NAL sin el código de inicio (cabecera incluida).
    pub data: &'a [u8],
}

impl Nal<'_> {
    /// `nal_unit_type` (5 bits bajos de la cabecera).
    pub fn nal_type(&self) -> u8 {
        self.data.first().map_or(0, |b| b & 0x1F)
    }

    /// `nal_ref_idc` (2 bits).
    pub fn ref_idc(&self) -> u8 {
        self.data.first().map_or(0, |b| (b >> 5) & 0x3)
    }

    /// `forbidden_zero_bit` debe ser 0.
    pub fn forbidden_bit(&self) -> bool {
        self.data.first().is_some_and(|b| b & 0x80 != 0)
    }

    /// Carga útil tras la cabecera de 1 byte (aún con bytes de prevención).
    pub fn payload(&self) -> &[u8] {
        self.data.get(1..).unwrap_or(&[])
    }
}

/// Busca el siguiente código de inicio `00 00 01` desde `from`.
/// Devuelve (posición del código, longitud 3 o 4).
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        // Salto rápido: si buf[i+2] > 1 no puede empezar un código en i ni i+1.
        if buf[i + 2] > 1 {
            i += 3;
            continue;
        }
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            if i > from && buf[i - 1] == 0 {
                return Some((i - 1, 4));
            }
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

/// ¿Empieza el búfer con un código de inicio Annex-B?
pub fn starts_with_start_code(buf: &[u8]) -> bool {
    buf.starts_with(&[0, 0, 1]) || buf.starts_with(&[0, 0, 0, 1])
}

/// Itera las NAL de un búfer Annex-B. Los bytes anteriores al primer código
/// de inicio se ignoran. Los ceros finales de una NAL (`trailing_zero_8bits`)
/// se recortan.
pub fn split_nals(buf: &[u8]) -> Vec<Nal<'_>> {
    let mut out = Vec::new();
    let Some((mut pos, mut sc_len)) = find_start_code(buf, 0) else {
        return out;
    };
    loop {
        let data_start = pos + sc_len;
        let next = find_start_code(buf, data_start);
        let mut data_end = next.map_or(buf.len(), |(p, _)| p);
        while data_end > data_start && buf[data_end - 1] == 0 {
            data_end -= 1;
        }
        if data_end > data_start {
            out.push(Nal {
                offset: pos,
                start_code_len: sc_len,
                data: &buf[data_start..data_end],
            });
        }
        match next {
            Some((p, l)) => {
                pos = p;
                sc_len = l;
            }
            None => break,
        }
    }
    out
}

/// Resumen de una unidad de acceso.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessUnitSummary {
    pub nal_types: Vec<u8>,
    pub has_sps: bool,
    pub has_pps: bool,
    pub has_idr: bool,
    pub has_non_idr: bool,
    /// Alguna NAL con `forbidden_zero_bit` a 1 (flujo corrupto).
    pub forbidden_bit_set: bool,
}

/// Clasifica las NAL de una unidad de acceso.
pub fn summarize(buf: &[u8]) -> AccessUnitSummary {
    let mut s = AccessUnitSummary::default();
    for nal in split_nals(buf) {
        let t = nal.nal_type();
        s.nal_types.push(t);
        s.has_sps |= t == nal_type::SPS;
        s.has_pps |= t == nal_type::PPS;
        s.has_idr |= t == nal_type::IDR_SLICE;
        s.has_non_idr |= t == nal_type::NON_IDR_SLICE;
        s.forbidden_bit_set |= nal.forbidden_bit();
    }
    s
}

/// Extrae SPS y PPS de un búfer Annex-B y los devuelve como Annex-B
/// (`00 00 00 01 SPS 00 00 00 01 PPS …`). Vacío si no hay ninguno.
pub fn extract_parameter_sets(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in split_nals(buf) {
        if matches!(nal.nal_type(), nal_type::SPS | nal_type::PPS) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal.data);
        }
    }
    out
}

/// Convierte un búfer AVCC (NAL precedidas de longitud big-endian de
/// `len_size` bytes) a Annex-B. `None` si las longitudes no cuadran.
pub fn avcc_to_annexb(buf: &[u8], len_size: usize) -> Option<Vec<u8>> {
    if !(1..=4).contains(&len_size) {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len() + 16);
    let mut i = 0;
    while i < buf.len() {
        if i + len_size > buf.len() {
            return None;
        }
        let mut n = 0usize;
        for b in &buf[i..i + len_size] {
            n = (n << 8) | *b as usize;
        }
        i += len_size;
        if n == 0 || i + n > buf.len() {
            return None;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&buf[i..i + n]);
        i += n;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes() {
        let buf = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, // SPS (4 bytes de código)
            0, 0, 1, 0x68, 0xEE, // PPS (3 bytes)
            0, 0, 0, 1, 0x65, 0x88, 0x00, 0x00, 0x03, 0x01, // IDR con prevención
        ];
        let nals = split_nals(&buf);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].nal_type(), nal_type::SPS);
        assert_eq!(nals[0].start_code_len, 4);
        assert_eq!(nals[0].data, &[0x67, 0x64]); // ceros finales recortados
        assert_eq!(nals[1].nal_type(), nal_type::PPS);
        // El 0x00 final del SPS + `00 00 01` se lee como código de 4 bytes
        // (un RBSP nunca termina en 0x00, así que es equivalente).
        assert_eq!(nals[1].start_code_len, 4);
        assert_eq!(nals[1].offset, 6);
        assert_eq!(nals[2].nal_type(), nal_type::IDR_SLICE);
        assert_eq!(nals[2].ref_idc(), 3);
        assert_eq!(nals[2].data, &[0x65, 0x88, 0x00, 0x00, 0x03, 0x01]);
    }

    #[test]
    fn ignores_leading_garbage_and_empty() {
        assert!(split_nals(&[]).is_empty());
        assert!(split_nals(&[1, 2, 3]).is_empty());
        let nals = split_nals(&[9, 9, 0, 0, 1, 0x41, 0xAA]);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].nal_type(), nal_type::NON_IDR_SLICE);
        assert_eq!(nals[0].offset, 2);
        assert_eq!(nals[0].payload(), &[0xAA]);
        // Código de inicio sin datos detrás.
        assert!(split_nals(&[0, 0, 0, 1]).is_empty());
    }

    #[test]
    fn summary_flags() {
        let buf = [
            0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1,
        ];
        let s = summarize(&buf);
        assert_eq!(s.nal_types, vec![9, 7, 8, 5]);
        assert!(s.has_sps && s.has_pps && s.has_idr && !s.has_non_idr);
        assert!(!s.forbidden_bit_set);
        assert!(starts_with_start_code(&buf));
        assert!(!starts_with_start_code(&buf[1..2]));
        let ps = extract_parameter_sets(&buf);
        assert_eq!(ps, vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1]);
    }

    #[test]
    fn avcc_conversion() {
        let avcc = [0, 0, 0, 2, 0x67, 0x42, 0, 0, 0, 1, 0x68];
        assert_eq!(
            avcc_to_annexb(&avcc, 4).unwrap(),
            vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68]
        );
        assert!(avcc_to_annexb(&[0, 0, 0, 9, 1], 4).is_none());
        assert!(avcc_to_annexb(&avcc, 0).is_none());
    }
}
