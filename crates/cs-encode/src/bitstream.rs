//! Análisis mínimo de los flujos codificados según el códec (ADR-0012).
//!
//! Lo justo para el emisor y el receptor:
//! - **fotograma clave:** ¿la unidad es un punto de acceso aleatorio?
//! - **parámetros de secuencia:** ¿la unidad los lleva y cuáles son? Así un
//!   fotograma clave sin ellos se puede completar con los últimos conocidos.
//!
//! Formatos:
//! - **H.264 / HEVC:** Annex-B (`00 00 01` + NAL). La cabecera NAL de H.264
//!   ocupa 1 byte (tipo en los 5 bits bajos); la de HEVC, 2 bytes (tipo en
//!   los bits 1–6 del primero).
//! - **AV1:** secuencia de OBU con campo de tamaño (*low overhead bitstream
//!   format*). Tipo en los bits 3–6 de la cabecera; el tamaño va en LEB128.

use crate::annexb::{self, split_nals};

/// Códec de vídeo (el del protocolo).
pub use cs_core::Codec as VideoCodec;

/// Tipos de NAL de HEVC (H.265 tabla 7-1).
pub mod hevc_nal {
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA: u8 = 21;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
}

/// Tipos de OBU de AV1 (sección 6.2.2).
pub mod av1_obu {
    pub const SEQUENCE_HEADER: u8 = 1;
    pub const TEMPORAL_DELIMITER: u8 = 2;
    pub const FRAME_HEADER: u8 = 3;
    pub const FRAME: u8 = 6;
}

/// Resumen de una unidad codificada.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnitInfo {
    /// Punto de acceso aleatorio (IDR en H.264/HEVC; en AV1, la presencia de
    /// la cabecera de secuencia, que el codificador pone en los clave).
    pub keyframe: bool,
    /// Lleva todos los parámetros de secuencia necesarios para decodificar
    /// desde aquí (SPS+PPS; VPS+SPS+PPS; cabecera de secuencia).
    pub has_parameter_sets: bool,
}

/// Tipo de NAL de HEVC.
fn hevc_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3F)
}

/// Una OBU de AV1: tipo y bytes completos (cabecera incluida).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub obu_type: u8,
    pub data: &'a [u8],
}

/// Lee un LEB128 (máx. 8 bytes). Devuelve (valor, bytes leídos).
fn leb128(buf: &[u8]) -> Option<(usize, usize)> {
    let mut v: u64 = 0;
    for (i, b) in buf.iter().take(8).enumerate() {
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((usize::try_from(v).ok()?, i + 1));
        }
    }
    None
}

/// Separa las OBU de un búfer AV1. Se detiene en la primera mal formada.
pub fn split_obus(buf: &[u8]) -> Vec<Obu<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let h = buf[i];
        if h & 0x80 != 0 {
            break; // forbidden bit
        }
        let obu_type = (h >> 3) & 0x0F;
        let has_ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let header_len = 1 + usize::from(has_ext);
        if !has_size {
            // Sin tamaño: ocupa el resto del búfer.
            out.push(Obu {
                obu_type,
                data: &buf[i..],
            });
            break;
        }
        let Some((size, n)) = buf.get(i + header_len..).and_then(leb128) else {
            break;
        };
        let end = i + header_len + n + size;
        if end > buf.len() {
            break;
        }
        out.push(Obu {
            obu_type,
            data: &buf[i..end],
        });
        i = end;
    }
    out
}

/// Clasifica una unidad codificada.
pub fn inspect(codec: VideoCodec, buf: &[u8]) -> UnitInfo {
    match codec {
        VideoCodec::H264 => {
            let s = annexb::summarize(buf);
            UnitInfo {
                keyframe: s.has_idr,
                has_parameter_sets: s.has_sps && s.has_pps,
            }
        }
        VideoCodec::Hevc => {
            let (mut vps, mut sps, mut pps, mut irap) = (false, false, false, false);
            for nal in split_nals(buf) {
                match hevc_type(nal.data) {
                    hevc_nal::VPS => vps = true,
                    hevc_nal::SPS => sps = true,
                    hevc_nal::PPS => pps = true,
                    hevc_nal::IDR_W_RADL | hevc_nal::IDR_N_LP | hevc_nal::CRA => irap = true,
                    _ => {}
                }
            }
            UnitInfo {
                keyframe: irap,
                has_parameter_sets: vps && sps && pps,
            }
        }
        VideoCodec::Av1 => {
            let seq = split_obus(buf)
                .iter()
                .any(|o| o.obu_type == av1_obu::SEQUENCE_HEADER);
            UnitInfo {
                keyframe: seq,
                has_parameter_sets: seq,
            }
        }
    }
}

/// Extrae los parámetros de secuencia de `buf` en el formato del códec
/// (para anteponerlos a un fotograma clave que no los traiga). Vacío si no
/// hay ninguno.
pub fn parameter_sets(codec: VideoCodec, buf: &[u8]) -> Vec<u8> {
    match codec {
        VideoCodec::H264 => annexb::extract_parameter_sets(buf),
        VideoCodec::Hevc => {
            let mut out = Vec::new();
            for nal in split_nals(buf) {
                if matches!(
                    hevc_type(nal.data),
                    hevc_nal::VPS | hevc_nal::SPS | hevc_nal::PPS
                ) {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal.data);
                }
            }
            out
        }
        VideoCodec::Av1 => split_obus(buf)
            .iter()
            .filter(|o| o.obu_type == av1_obu::SEQUENCE_HEADER)
            .flat_map(|o| o.data.iter().copied())
            .collect(),
    }
}

/// Antepone `params` a una unidad clave que no los lleve. En AV1, la
/// cabecera de secuencia va tras el delimitador temporal, si lo hay.
pub fn prepend_parameter_sets(codec: VideoCodec, params: &[u8], unit: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(params.len() + unit.len());
    if codec == VideoCodec::Av1 {
        if let Some(td) = split_obus(unit)
            .first()
            .filter(|o| o.obu_type == av1_obu::TEMPORAL_DELIMITER)
        {
            let n = td.data.len();
            v.extend_from_slice(&unit[..n]);
            v.extend_from_slice(params);
            v.extend_from_slice(&unit[n..]);
            return v;
        }
    }
    v.extend_from_slice(params);
    v.extend_from_slice(unit);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(sc: &[u8], header: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = sc.to_vec();
        v.extend_from_slice(header);
        v.extend_from_slice(body);
        v
    }

    /// Cabecera NAL de HEVC para `t` (layer 0, tid 1).
    fn hevc(t: u8) -> [u8; 2] {
        [t << 1, 1]
    }

    #[test]
    fn hevc_keyframes_and_parameter_sets() {
        let mut key = Vec::new();
        for t in [hevc_nal::VPS, hevc_nal::SPS, hevc_nal::PPS] {
            key.extend(nal(&[0, 0, 0, 1], &hevc(t), &[0xAA, 0xBB]));
        }
        key.extend(nal(&[0, 0, 1], &hevc(hevc_nal::IDR_W_RADL), &[0x11; 20]));
        let i = inspect(VideoCodec::Hevc, &key);
        assert!(i.keyframe && i.has_parameter_sets);

        // P-frame (TRAIL_R = 1).
        let p = nal(&[0, 0, 0, 1], &hevc(1), &[0x22; 10]);
        assert_eq!(inspect(VideoCodec::Hevc, &p), UnitInfo::default());

        // Extraer y anteponer.
        let ps = parameter_sets(VideoCodec::Hevc, &key);
        assert_eq!(split_nals(&ps).len(), 3);
        let idr = nal(&[0, 0, 1], &hevc(hevc_nal::IDR_N_LP), &[0x33; 8]);
        let bare = inspect(VideoCodec::Hevc, &idr);
        assert!(bare.keyframe && !bare.has_parameter_sets);
        let fixed = prepend_parameter_sets(VideoCodec::Hevc, &ps, &idr);
        assert!(inspect(VideoCodec::Hevc, &fixed).has_parameter_sets);
    }

    #[test]
    fn hevc_type_is_not_confused_with_h264() {
        // 0x40 = VPS en HEVC; en H.264 sería tipo 0 (no especificado).
        let buf = nal(&[0, 0, 0, 1], &hevc(hevc_nal::VPS), &[1]);
        assert!(!inspect(VideoCodec::H264, &buf).keyframe);
        // 0x65 = IDR en H.264; en HEVC es tipo 50 (no IRAP).
        let h264_idr = nal(&[0, 0, 0, 1], &[0x65], &[1, 2, 3]);
        assert!(inspect(VideoCodec::H264, &h264_idr).keyframe);
        assert!(!inspect(VideoCodec::Hevc, &h264_idr).keyframe);
    }

    /// OBU con campo de tamaño.
    fn obu(t: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(t << 3) | 0x02];
        let mut n = payload.len();
        loop {
            let b = (n & 0x7F) as u8;
            n >>= 7;
            if n == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn av1_obus_are_split_and_classified() {
        let mut key = obu(av1_obu::TEMPORAL_DELIMITER, &[]);
        key.extend(obu(av1_obu::SEQUENCE_HEADER, &[0, 0, 0, 0x0A]));
        key.extend(obu(av1_obu::FRAME, &[0x55; 300])); // tamaño de 2 bytes LEB128
        let obus = split_obus(&key);
        assert_eq!(
            obus.iter().map(|o| o.obu_type).collect::<Vec<_>>(),
            vec![2, 1, 6]
        );
        let i = inspect(VideoCodec::Av1, &key);
        assert!(i.keyframe && i.has_parameter_sets);

        let mut inter = obu(av1_obu::TEMPORAL_DELIMITER, &[]);
        inter.extend(obu(av1_obu::FRAME, &[0x66; 40]));
        assert!(!inspect(VideoCodec::Av1, &inter).keyframe);

        // La cabecera de secuencia se inserta tras el delimitador temporal.
        let ps = parameter_sets(VideoCodec::Av1, &key);
        let fixed = prepend_parameter_sets(VideoCodec::Av1, &ps, &inter);
        let types: Vec<u8> = split_obus(&fixed).iter().map(|o| o.obu_type).collect();
        assert_eq!(types, vec![2, 1, 6]);
    }

    #[test]
    fn malformed_av1_does_not_panic() {
        assert!(split_obus(&[0x12, 0xFF, 0xFF, 0xFF]).is_empty());
        assert!(split_obus(&[0x80]).is_empty());
        assert_eq!(inspect(VideoCodec::Av1, &[]), UnitInfo::default());
    }
}
