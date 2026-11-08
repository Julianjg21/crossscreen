//! Análisis mínimo de H.264: SPS (con VUI), inicio de PPS y de la cabecera
//! de *slice*. Sirve para validar la salida del codificador (dimensiones,
//! perfil, ausencia de frames B, `max_num_reorder_frames`) sin decodificar.

/// Error de análisis del flujo de bits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("fin de datos inesperado")]
    Eof,
    #[error("valor exp-Golomb fuera de rango")]
    BadExpGolomb,
    #[error("NAL de tipo {0} inesperada")]
    WrongNalType(u8),
}

/// Quita los bytes de prevención de emulación (`00 00 03` → `00 00`).
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Lector de bits MSB primero sobre un RBSP ya sin prevención.
#[derive(Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Result<bool, ParseError> {
        let byte = *self.data.get(self.pos / 8).ok_or(ParseError::Eof)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(v == 1)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32, ParseError> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Ok(v)
    }

    /// `ue(v)`.
    pub fn ue(&mut self) -> Result<u32, ParseError> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(ParseError::BadExpGolomb);
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)? as u64;
        let v = (1u64 << zeros) - 1 + rest;
        u32::try_from(v).map_err(|_| ParseError::BadExpGolomb)
    }

    /// `se(v)`.
    pub fn se(&mut self) -> Result<i32, ParseError> {
        let k = self.ue()? as i64;
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(v as i32)
    }

    pub fn bits_read(&self) -> usize {
        self.pos
    }
}

/// Parámetros HRD resumidos (primer CPB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hrd {
    pub bit_rate_bps: u64,
    pub cpb_size_bits: u64,
    pub cbr: bool,
}

/// VUI resumida.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vui {
    pub video_full_range: Option<bool>,
    /// (colour_primaries, transfer_characteristics, matrix_coefficients).
    pub colour_description: Option<(u8, u8, u8)>,
    /// (num_units_in_tick, time_scale, fixed_frame_rate_flag).
    pub timing: Option<(u32, u32, bool)>,
    pub nal_hrd: Option<Hrd>,
    pub vcl_hrd: Option<Hrd>,
    pub low_delay_hrd: Option<bool>,
    pub pic_struct_present: bool,
    /// `max_num_reorder_frames`, si el SPS trae `bitstream_restriction`.
    /// Con 0 un decodificador puede sacar cada frame sin esperar.
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

/// SPS analizado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub pic_order_cnt_type: u32,
    pub max_num_ref_frames: u32,
    pub frame_mbs_only: bool,
    /// Dimensiones en macrobloques ×16 (antes del recorte).
    pub coded_width: u32,
    pub coded_height: u32,
    /// Dimensiones visibles tras `frame_cropping`.
    pub width: u32,
    pub height: u32,
    pub vui: Option<Vui>,
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Result<(), ParseError> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

fn parse_hrd(r: &mut BitReader<'_>) -> Result<Hrd, ParseError> {
    let cpb_cnt = r.ue()? + 1;
    let bit_rate_scale = r.bits(4)?;
    let cpb_size_scale = r.bits(4)?;
    let mut first: Option<Hrd> = None;
    for _ in 0..cpb_cnt {
        let br = r.ue()? as u64 + 1;
        let cs = r.ue()? as u64 + 1;
        let cbr = r.bit()?;
        if first.is_none() {
            first = Some(Hrd {
                bit_rate_bps: br << (6 + bit_rate_scale),
                cpb_size_bits: cs << (4 + cpb_size_scale),
                cbr,
            });
        }
    }
    r.bits(5)?; // initial_cpb_removal_delay_length_minus1
    r.bits(5)?; // cpb_removal_delay_length_minus1
    r.bits(5)?; // dpb_output_delay_length_minus1
    r.bits(5)?; // time_offset_length
    first.ok_or(ParseError::Eof)
}

fn parse_vui(r: &mut BitReader<'_>) -> Result<Vui, ParseError> {
    let mut v = Vui::default();
    if r.bit()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.bits(16)?;
            r.bits(16)?;
        }
    }
    if r.bit()? {
        r.bit()?; // overscan_appropriate_flag
    }
    if r.bit()? {
        // video_signal_type_present_flag
        r.bits(3)?; // video_format
        v.video_full_range = Some(r.bit()?);
        if r.bit()? {
            let p = r.bits(8)? as u8;
            let t = r.bits(8)? as u8;
            let m = r.bits(8)? as u8;
            v.colour_description = Some((p, t, m));
        }
    }
    if r.bit()? {
        r.ue()?; // chroma_sample_loc_type_top_field
        r.ue()?; // chroma_sample_loc_type_bottom_field
    }
    if r.bit()? {
        let n = r.bits(32)?;
        let ts = r.bits(32)?;
        let fixed = r.bit()?;
        v.timing = Some((n, ts, fixed));
    }
    if r.bit()? {
        v.nal_hrd = Some(parse_hrd(r)?);
    }
    if r.bit()? {
        v.vcl_hrd = Some(parse_hrd(r)?);
    }
    if v.nal_hrd.is_some() || v.vcl_hrd.is_some() {
        v.low_delay_hrd = Some(r.bit()?);
    }
    v.pic_struct_present = r.bit()?;
    if r.bit()? {
        // bitstream_restriction_flag
        r.bit()?; // motion_vectors_over_pic_boundaries_flag
        r.ue()?; // max_bytes_per_pic_denom
        r.ue()?; // max_bits_per_mb_denom
        r.ue()?; // log2_max_mv_length_horizontal
        r.ue()?; // log2_max_mv_length_vertical
        v.max_num_reorder_frames = Some(r.ue()?);
        v.max_dec_frame_buffering = Some(r.ue()?);
    }
    Ok(v)
}

/// Analiza una NAL SPS completa (cabecera incluida, con prevención).
pub fn parse_sps(nal: &[u8]) -> Result<Sps, ParseError> {
    let header = *nal.first().ok_or(ParseError::Eof)?;
    if header & 0x1F != 7 {
        return Err(ParseError::WrongNalType(header & 0x1F));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8)? as u8;
    let constraint_flags = r.bits(8)? as u8;
    let level_idc = r.bits(8)? as u8;
    let sps_id = r.ue()?;
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma = 8;
    let mut bit_depth_chroma = 8;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()?;
        }
        bit_depth_luma = r.ue()? + 8;
        bit_depth_chroma = r.ue()? + 8;
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            // seq_scaling_matrix_present_flag
            let n = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..n {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.ue()?;
    match pic_order_cnt_type {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = r.ue()? + 1;
    let h_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let vui = if r.bit()? {
        Some(parse_vui(&mut r)?)
    } else {
        None
    };

    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let coded_width = w_mbs * 16;
    let coded_height = h_map_units * 16 * field_factor;
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (crop_x, crop_y) = match chroma_array_type {
        0 => (1, field_factor),
        1 => (2, 2 * field_factor),
        2 => (2, field_factor),
        _ => (1, field_factor),
    };
    let width = coded_width.saturating_sub(crop_x * (cl + cr));
    let height = coded_height.saturating_sub(crop_y * (ct + cb));
    Ok(Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        sps_id,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        pic_order_cnt_type,
        max_num_ref_frames,
        frame_mbs_only,
        coded_width,
        coded_height,
        width,
        height,
        vui,
    })
}

/// Inicio de un PPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PpsStart {
    pub pps_id: u32,
    pub sps_id: u32,
    /// CABAC (`entropy_coding_mode_flag`).
    pub cabac: bool,
}

/// Analiza el inicio de una NAL PPS.
pub fn parse_pps_start(nal: &[u8]) -> Result<PpsStart, ParseError> {
    let header = *nal.first().ok_or(ParseError::Eof)?;
    if header & 0x1F != 8 {
        return Err(ParseError::WrongNalType(header & 0x1F));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    Ok(PpsStart {
        pps_id: r.ue()?,
        sps_id: r.ue()?,
        cabac: r.bit()?,
    })
}

/// Tipo de *slice* (`slice_type % 5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

/// Inicio de la cabecera de un *slice*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceStart {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
}

/// Analiza el inicio de una NAL de *slice* (tipos 1 y 5).
pub fn parse_slice_start(nal: &[u8]) -> Result<SliceStart, ParseError> {
    let header = *nal.first().ok_or(ParseError::Eof)?;
    let t = header & 0x1F;
    if t != 1 && t != 5 {
        return Err(ParseError::WrongNalType(t));
    }
    // Basta con los primeros bytes; evita copiar todo el slice.
    let head = &nal[1..nal.len().min(33)];
    let rbsp = unescape_rbsp(head);
    let mut r = BitReader::new(&rbsp);
    let first_mb_in_slice = r.ue()?;
    let slice_type = match r.ue()? % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        3 => SliceType::Sp,
        _ => SliceType::Si,
    };
    let pps_id = r.ue()?;
    Ok(SliceStart {
        first_mb_in_slice,
        slice_type,
        pps_id,
    })
}

/// Mensaje SEI (H.264 §7.3.2.3.1): `(payload_type, payload_size)`.
/// `payload_type == 6` es `recovery_point` (usado por *gradual intra refresh*).
pub fn parse_sei_messages(nal: &[u8]) -> Result<Vec<(u32, u32)>, ParseError> {
    let header = *nal.first().ok_or(ParseError::Eof)?;
    if header & 0x1F != 6 {
        return Err(ParseError::WrongNalType(header & 0x1F));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut i = 0usize;
    let mut out = Vec::new();
    // El último byte es `rbsp_trailing_bits` (0x80) cuando queda 1 byte.
    while i < rbsp.len() && !(i + 1 == rbsp.len() && rbsp[i] == 0x80) {
        let mut payload_type = 0u32;
        while *rbsp.get(i).ok_or(ParseError::Eof)? == 0xFF {
            payload_type = payload_type.saturating_add(255);
            i += 1;
        }
        payload_type = payload_type.saturating_add(u32::from(*rbsp.get(i).ok_or(ParseError::Eof)?));
        i += 1;

        let mut payload_size = 0u32;
        while *rbsp.get(i).ok_or(ParseError::Eof)? == 0xFF {
            payload_size = payload_size.saturating_add(255);
            i += 1;
        }
        payload_size = payload_size.saturating_add(u32::from(*rbsp.get(i).ok_or(ParseError::Eof)?));
        i += 1;

        let end = i.saturating_add(payload_size as usize);
        if end > rbsp.len() {
            return Err(ParseError::Eof);
        }
        out.push((payload_type, payload_size));
        i = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escritor de bits solo para pruebas (genera SPS sintéticos).
    struct BitWriter {
        out: Vec<u8>,
        nbits: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                nbits: 0,
            }
        }
        fn bit(&mut self, b: bool) {
            if self.nbits % 8 == 0 {
                self.out.push(0);
            }
            if b {
                let last = self.out.last_mut().unwrap();
                *last |= 1 << (7 - (self.nbits % 8));
            }
            self.nbits += 1;
        }
        fn bits(&mut self, n: u32, v: u32) {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1 == 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v as u64 + 1;
            let len = 64 - x.leading_zeros();
            for _ in 0..len - 1 {
                self.bit(false);
            }
            for i in (0..len).rev() {
                self.bit((x >> i) & 1 == 1);
            }
        }
        fn se(&mut self, v: i32) {
            let k = if v > 0 { 2 * v - 1 } else { -2 * v };
            self.ue(k as u32);
        }
        /// rbsp_trailing_bits y prevención de emulación.
        fn finish_nal(mut self, header: u8) -> Vec<u8> {
            self.bit(true);
            while self.nbits % 8 != 0 {
                self.bit(false);
            }
            let mut nal = vec![header];
            let mut zeros = 0;
            for b in self.out {
                if zeros >= 2 && b <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                nal.push(b);
            }
            nal
        }
    }

    #[test]
    fn exp_golomb_roundtrip() {
        let mut w = BitWriter::new();
        let vals = [0u32, 1, 2, 3, 7, 8, 255, 1919, 65535, 1 << 20];
        for v in vals {
            w.ue(v);
        }
        for v in [-5i32, 0, 1, -1, 1000] {
            w.se(v);
        }
        let data = w.out;
        let mut r = BitReader::new(&data);
        for v in vals {
            assert_eq!(r.ue().unwrap(), v);
        }
        for v in [-5i32, 0, 1, -1, 1000] {
            assert_eq!(r.se().unwrap(), v);
        }
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(
            unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]),
            vec![0, 0, 1, 0, 0, 0]
        );
        assert_eq!(unescape_rbsp(&[0, 3, 0]), vec![0, 3, 0]);
    }

    #[test]
    fn parses_synthetic_sps() {
        let nal = synthetic_sps_1080p();
        let sps = parse_sps(&nal).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 42);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!((sps.coded_width, sps.coded_height), (1920, 1088));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.max_num_ref_frames, 1);
        let vui = sps.vui.unwrap();
        assert_eq!(vui.video_full_range, Some(false));
        assert_eq!(vui.colour_description, Some((1, 1, 1)));
        assert_eq!(vui.timing, Some((1, 120, true)));
        let hrd = vui.nal_hrd.unwrap();
        assert_eq!(hrd.bit_rate_bps, 20_000_000);
        assert!(hrd.cbr);
        assert_eq!(vui.low_delay_hrd, Some(true));
        assert_eq!(vui.max_num_reorder_frames, Some(0));
        assert_eq!(vui.max_dec_frame_buffering, Some(1));
    }

    /// SPS High 1920×1080 (1088 codificado + recorte de 8 líneas) con listas
    /// de escalado, VUI de rango limitado BT.709, temporización a 60 fps, HRD
    /// CBR de 20 Mbps y `max_num_reorder_frames = 0`.
    fn synthetic_sps_1080p() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(8, 100);
        w.bits(8, 0);
        w.bits(8, 42);
        w.ue(0);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.bit(false);
        w.bit(true); // seq_scaling_matrix_present_flag
        for i in 0..8 {
            w.bit(i == 0 || i == 6);
            if i == 0 {
                // 16 coeficientes: delta 2 (next=10), luego 0×14, luego -10 → next=0.
                w.se(2);
                for _ in 0..14 {
                    w.se(0);
                }
                w.se(-10);
            }
            if i == 6 {
                // 64 coeficientes: -8 → next=0 en el primero; no hay más deltas.
                w.se(-8);
            }
        }
        w.ue(0);
        w.ue(0); // poc type 0
        w.ue(2); // log2_max_poc_lsb_minus4
        w.ue(1);
        w.bit(false);
        w.ue(119);
        w.ue(67);
        w.bit(true);
        w.bit(true);
        w.bit(true);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4);
        w.bit(true);
        w.bit(false);
        w.bit(false);
        w.bit(true);
        w.bits(3, 5);
        w.bit(false);
        w.bit(true);
        w.bits(8, 1);
        w.bits(8, 1);
        w.bits(8, 1);
        w.bit(false);
        w.bit(true);
        w.bits(32, 1);
        w.bits(32, 120);
        w.bit(true);
        w.bit(true);
        w.ue(0);
        w.bits(4, 0);
        w.bits(4, 0);
        w.ue(312_499);
        w.ue(83_332);
        w.bit(true);
        w.bits(5, 23);
        w.bits(5, 23);
        w.bits(5, 23);
        w.bits(5, 24);
        w.bit(false);
        w.bit(true);
        w.bit(false);
        w.bit(true);
        w.bit(true);
        w.ue(0);
        w.ue(0);
        w.ue(16);
        w.ue(16);
        w.ue(0);
        w.ue(1);
        w.finish_nal(0x67)
    }

    #[test]
    fn parses_baseline_without_vui() {
        let mut w = BitWriter::new();
        w.bits(8, 66);
        w.bits(8, 0xC0);
        w.bits(8, 31);
        w.ue(0);
        w.ue(0);
        w.ue(2);
        w.ue(1);
        w.bit(false);
        w.ue(79); // 1280
        w.ue(44); // 720
        w.bit(true);
        w.bit(true);
        w.bit(false);
        w.bit(false); // sin VUI
        let nal = w.finish_nal(0x67);
        let sps = parse_sps(&nal).unwrap();
        assert_eq!((sps.width, sps.height), (1280, 720));
        assert_eq!(sps.constraint_flags, 0xC0);
        assert!(sps.vui.is_none());
        assert_eq!(parse_sps(&[0x68, 0]), Err(ParseError::WrongNalType(8)));
        assert_eq!(parse_sps(&[0x67, 100]), Err(ParseError::Eof));
    }

    #[test]
    fn parses_pps_and_slice_start() {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.bit(true);
        let pps = w.finish_nal(0x68);
        assert_eq!(
            parse_pps_start(&pps).unwrap(),
            PpsStart {
                pps_id: 0,
                sps_id: 0,
                cabac: true
            }
        );

        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(7); // I (7 % 5 = 2)
        w.ue(0);
        let idr = w.finish_nal(0x65);
        let s = parse_slice_start(&idr).unwrap();
        assert_eq!(s.slice_type, SliceType::I);
        assert_eq!(s.first_mb_in_slice, 0);

        let mut w = BitWriter::new();
        w.ue(120);
        w.ue(5); // P
        w.ue(0);
        let p = w.finish_nal(0x41);
        let s = parse_slice_start(&p).unwrap();
        assert_eq!(s.slice_type, SliceType::P);
        assert_eq!(s.first_mb_in_slice, 120);
        assert!(parse_slice_start(&[0x67, 0]).is_err());

        // SEI con dos mensajes: buffering_period (0, 3 B) y recovery_point (6, 2 B) + trailing 0x80.
        let sei = [
            0x06, 0x00, 0x03, 0xAA, 0xBB, 0xCC, 0x06, 0x02, 0x10, 0x20, 0x80,
        ];
        assert_eq!(parse_sei_messages(&sei).unwrap(), vec![(0, 3), (6, 2)]);
        assert!(parse_sei_messages(&[0x67, 0x80]).is_err());
    }

    /// SPS real emitido por el MFT Intel Quick Sync (Iris Xe) en la PoC-3
    /// para 1920×1080 @ 60 fps, High, CBR 20 Mbps.
    #[test]
    fn parses_real_intel_qsv_sps() {
        let nal = [
            0x27, 0x64, 0x00, 0x2a, 0xac, 0x2b, 0x40, 0x3c, 0x01, 0x13, 0xf2, 0xe0, 0x28, 0x10,
            0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x07, 0x8e, 0x84, 0x00, 0x04, 0xc4, 0xb0,
            0x00, 0x14, 0x53, 0x3b, 0xde, 0xf8, 0x3b, 0x43, 0x86, 0x54,
        ];
        let sps = parse_sps(&nal).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 42);
        assert_eq!((sps.coded_width, sps.coded_height), (1920, 1088));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.max_num_ref_frames, 1);
        assert_eq!(sps.pic_order_cnt_type, 2);
        let vui = sps.vui.unwrap();
        assert_eq!(vui.video_full_range, Some(false));
        assert_eq!(vui.timing, Some((1, 120, true)));
        assert_eq!(vui.max_num_reorder_frames, Some(0));
        assert_eq!(vui.max_dec_frame_buffering, Some(1));
        let hrd = vui.nal_hrd.unwrap();
        assert!(hrd.cbr);
        assert_eq!(hrd.bit_rate_bps, 19_999_744);
    }
}
