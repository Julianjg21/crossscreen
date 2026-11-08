//! Utilidades para flujos H.264 en formato Annex-B (códigos de inicio
//! `00 00 01` / `00 00 00 01`).
//!
//! En producción el visor recibe ya una *access unit* (AU) por
//! `CompleteFrame` del reensamblador de `cs-transport`. Para reproducir un
//! fichero `.h264` crudo hace falta partirlo en AUs: [`split_access_units`]
//! lo hace con las reglas de H.264 §7.4.1.2.3 simplificadas:
//!
//! - Un AUD (tipo 9) siempre abre una AU nueva.
//! - Tras haber visto un slice (VCL) en la AU actual, abren AU nueva: SEI (6),
//!   SPS (7), PPS (8), tipos 14–18, y cualquier slice (1 o 5) con
//!   `first_mb_in_slice == 0`.
//!
//! La heurística de `first_mb_in_slice` cubre flujos con uno o varios slices
//! por imagen (el primer slice de cada imagen empieza en el macrobloque 0).
//! No cubre ASO (orden arbitrario de slices, solo perfil Baseline extendido),
//! que ningún codificador de los que usamos emite.

/// Tipos de NAL relevantes (H.264 tabla 7-1).
pub mod nal_type {
    /// Slice no IDR.
    pub const SLICE: u8 = 1;
    /// Slice IDR.
    pub const IDR: u8 = 5;
    /// SEI.
    pub const SEI: u8 = 6;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Access unit delimiter.
    pub const AUD: u8 = 9;
}

/// Una NAL dentro de un búfer Annex-B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// Desplazamiento del código de inicio (incluido el cero inicial de los
    /// códigos de 4 bytes) dentro del búfer original.
    pub start_code_offset: usize,
    /// Bytes de la NAL sin código de inicio (empieza por la cabecera NAL).
    pub data: &'a [u8],
}

impl NalUnit<'_> {
    /// `nal_unit_type` (5 bits bajos de la cabecera).
    pub fn nal_type(&self) -> u8 {
        self.data.first().map_or(0, |b| b & 0x1f)
    }

    /// ¿Es una NAL de slice (VCL) de imagen primaria?
    pub fn is_vcl(&self) -> bool {
        matches!(self.nal_type(), 1..=5)
    }

    /// Para un slice: ¿`first_mb_in_slice == 0`? Es `ue(v)` y vale 0 solo si
    /// su primer bit es `1`, es decir, si el bit alto del primer byte tras la
    /// cabecera está puesto. (La cabecera de slice no puede contener bytes de
    /// prevención de emulación en su primer byte.)
    pub fn first_mb_is_zero(&self) -> bool {
        self.data.get(1).is_some_and(|b| b & 0x80 != 0)
    }
}

/// Itera las NAL de un búfer Annex-B. Los bytes anteriores al primer código
/// de inicio se ignoran.
pub fn nal_units(data: &[u8]) -> Vec<NalUnit<'_>> {
    // Posiciones (inicio del código, inicio de la carga) de cada código.
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(sc, payload)) in starts.iter().enumerate() {
        let mut end = starts
            .get(k + 1)
            .map_or(data.len(), |&(next_sc, _)| next_sc);
        // Quitar trailing_zero_8bits que preceden al siguiente código.
        while end > payload && data[end - 1] == 0 {
            end -= 1;
        }
        if end > payload {
            out.push(NalUnit {
                start_code_offset: sc,
                data: &data[payload..end],
            });
        }
    }
    out
}

/// Parte un flujo Annex-B en *access units*. Cada AU devuelta es un subslice
/// contiguo del búfer original que incluye sus códigos de inicio, lista para
/// pasarla al decodificador.
pub fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let nals = nal_units(data);
    let mut aus = Vec::new();
    let mut au_start: Option<usize> = None;
    let mut seen_vcl = false;
    for nal in &nals {
        let t = nal.nal_type();
        let starts_new = match au_start {
            None => true,
            Some(_) if t == nal_type::AUD => true,
            Some(_) if seen_vcl => match t {
                nal_type::SEI | nal_type::SPS | nal_type::PPS | 14..=18 => true,
                1..=5 => nal.first_mb_is_zero(),
                _ => false,
            },
            Some(_) => false,
        };
        if starts_new {
            if let Some(s) = au_start {
                aus.push(&data[s..nal.start_code_offset]);
            }
            au_start = Some(nal.start_code_offset);
            seen_vcl = false;
        }
        if nal.is_vcl() {
            seen_vcl = true;
        }
    }
    if let Some(s) = au_start {
        aus.push(&data[s..]);
    }
    aus
}

/// Tipos de NAL de una AU, en orden (útil para diagnóstico y tests).
pub fn nal_types(au: &[u8]) -> Vec<u8> {
    nal_units(au).iter().map(NalUnit::nal_type).collect()
}

/// ¿Contiene la AU un slice IDR?
pub fn is_idr(au: &[u8]) -> bool {
    nal_units(au).iter().any(|n| n.nal_type() == nal_type::IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construye una NAL con código de inicio de 4 bytes.
    fn nal(header: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, header];
        v.extend_from_slice(body);
        v
    }

    fn sps() -> Vec<u8> {
        nal(0x67, &[0x42, 0xc0, 0x1f])
    }
    fn pps() -> Vec<u8> {
        nal(0x68, &[0xce, 0x3c, 0x80])
    }
    fn aud() -> Vec<u8> {
        nal(0x09, &[0xf0])
    }
    /// Slice IDR con first_mb_in_slice = 0.
    fn idr0() -> Vec<u8> {
        nal(0x65, &[0x88, 0x84, 0x00])
    }
    /// Slice P con first_mb_in_slice = 0.
    fn p0() -> Vec<u8> {
        nal(0x41, &[0x9a, 0x02, 0x03])
    }
    /// Slice P con first_mb_in_slice != 0 (segundo slice de la imagen).
    fn p_cont() -> Vec<u8> {
        nal(0x41, &[0x2a, 0x02, 0x03])
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn nal_parsing_with_3_and_4_byte_start_codes() {
        let mut data = vec![0, 0, 1, 0x67, 0x42];
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce]);
        let nals = nal_units(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].start_code_offset, 0);
        assert_eq!(nals[0].data, &[0x67, 0x42]);
        assert_eq!(nals[1].start_code_offset, 5);
        assert_eq!(nals[1].data, &[0x68, 0xce]);
    }

    #[test]
    fn split_by_aud() {
        let data = cat(&[aud(), sps(), pps(), idr0(), aud(), p0(), aud(), p0()]);
        let aus = split_access_units(&data);
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_types(aus[0]), vec![9, 7, 8, 5]);
        assert_eq!(nal_types(aus[1]), vec![9, 1]);
        assert_eq!(nal_types(aus[2]), vec![9, 1]);
        assert!(is_idr(aus[0]));
        assert!(!is_idr(aus[1]));
        // Las AUs cubren el búfer completo sin huecos.
        assert_eq!(aus.iter().map(|a| a.len()).sum::<usize>(), data.len());
    }

    #[test]
    fn split_without_aud_uses_first_mb_and_parameter_sets() {
        let data = cat(&[
            sps(),
            pps(),
            idr0(),
            p0(),
            p_cont(),
            p0(),
            sps(),
            pps(),
            idr0(),
        ]);
        let aus = split_access_units(&data);
        let types: Vec<_> = aus.iter().map(|a| nal_types(a)).collect();
        assert_eq!(
            types,
            vec![vec![7, 8, 5], vec![1, 1], vec![1], vec![7, 8, 5]]
        );
    }

    #[test]
    fn sei_before_slice_stays_in_same_au() {
        let sei = nal(0x06, &[0x05, 0x01, 0xff, 0x80]);
        let data = cat(&[sps(), pps(), sei.clone(), idr0(), sei, p0()]);
        let aus = split_access_units(&data);
        assert_eq!(aus.len(), 2);
        assert_eq!(nal_types(aus[0]), vec![7, 8, 6, 5]);
        assert_eq!(nal_types(aus[1]), vec![6, 1]);
    }

    #[test]
    fn trailing_zeros_and_garbage_prefix() {
        let mut data = vec![0xde, 0xad];
        data.extend(idr0());
        data.extend([0, 0]);
        data.extend(p0());
        let aus = split_access_units(&data);
        assert_eq!(aus.len(), 2);
        assert_eq!(nal_units(aus[0])[0].data, &[0x65, 0x88, 0x84]);
    }

    #[test]
    fn empty_input() {
        assert!(split_access_units(&[]).is_empty());
        assert!(split_access_units(&[0, 0, 0]).is_empty());
    }
}
