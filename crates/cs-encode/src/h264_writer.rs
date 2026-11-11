//! Escritura de cabeceras H.264 (SPS, PPS y cabecera de *slice*) para los
//! codificadores por hardware que piden las cabeceras empaquetadas a la
//! aplicación (VAAPI con `VA_ENC_PACKED_HEADER_*`).
//!
//! Flujo de **baja latencia** para escritorio remoto:
//! - perfil High 4:2:0 8 bits, CABAC y transformada 8×8;
//! - sin frames B y **una sola referencia** (el frame anterior);
//! - `pic_order_cnt_type = 2` (orden de salida = orden de decodificación);
//! - VUI con `max_num_reorder_frames = 0` y `max_dec_frame_buffering = 1`:
//!   el decodificador puede mostrar cada frame en cuanto lo recibe;
//! - color BT.709 de rango limitado (lo que produce `cs_capture::yuv`).

/// Bits de `frame_num` (`log2_max_frame_num`). Corto a propósito: menos bytes
/// a cero en la cabecera de *slice* (ver [`slice_header`]).
pub const LOG2_MAX_FRAME_NUM: u32 = 8;
/// QP inicial del PPS; cada *slice* lo ajusta con `slice_qp_delta`.
pub const PIC_INIT_QP: i32 = 26;

/// Escritor de bits MSB primero.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits ocupados del último byte (0 = byte completo o vacío).
    used: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bit(&mut self, b: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if b {
            let last = self.bytes.last_mut().expect("hay un byte");
            *last |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    /// Los `n` bits bajos de `v` (n ≤ 32).
    pub fn bits(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Exp-Golomb sin signo.
    pub fn ue(&mut self, v: u32) {
        let v1 = u64::from(v) + 1;
        let len = 64 - v1.leading_zeros(); // bits de v+1
        for _ in 0..len - 1 {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((v1 >> i) & 1 == 1);
        }
    }

    /// Exp-Golomb con signo.
    pub fn se(&mut self, v: i32) {
        let mapped = if v > 0 {
            (v as u32) * 2 - 1
        } else {
            v.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// `rbsp_trailing_bits`: un 1 y ceros hasta alinear.
    pub fn trailing(&mut self) {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
    }

    pub fn bit_len(&self) -> usize {
        if self.used == 0 {
            self.bytes.len() * 8
        } else {
            (self.bytes.len() - 1) * 8 + self.used as usize
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Inserta los bytes de prevención de emulación (`00 00 0x` → `00 00 03 0x`
/// para `x ≤ 3`) en una RBSP.
pub fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0usize;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Prefijo Annex-B de cada NAL.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Parámetros del flujo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl StreamParams {
    pub fn width_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub fn height_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    /// Nivel mínimo que admite el tamaño y la frecuencia (tabla A-1).
    pub fn level_idc(&self) -> u8 {
        let fs = u64::from(self.width_mbs()) * u64::from(self.height_mbs());
        let mbps = fs * u64::from(self.fps.max(1));
        // (nivel, MaxFS, MaxMBPS)
        const LEVELS: [(u8, u64, u64); 10] = [
            (30, 1_620, 40_500),
            (31, 3_600, 108_000),
            (32, 5_120, 216_000),
            (40, 8_192, 245_760),
            (42, 8_704, 522_240),
            (50, 22_080, 589_824),
            (51, 36_864, 983_040),
            (52, 36_864, 2_073_600),
            (60, 139_264, 4_177_920),
            (62, 139_264, 16_711_680),
        ];
        LEVELS
            .iter()
            .find(|(_, max_fs, max_mbps)| fs <= *max_fs && mbps <= *max_mbps)
            .map(|l| l.0)
            .unwrap_or(62)
    }
}

/// `profile_idc` del perfil High.
pub const PROFILE_HIGH: u8 = 100;

fn nal(header: u8, w: BitWriter) -> Vec<u8> {
    let mut out = START_CODE.to_vec();
    out.push(header);
    out.extend_from_slice(&escape_rbsp(&w.into_bytes()));
    out
}

/// SPS completo (Annex-B: con código de inicio y escapado).
pub fn sps(p: &StreamParams) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(8, u32::from(PROFILE_HIGH));
    w.bits(8, 0); // constraint_set0..5 + reserved
    w.bits(8, u32::from(p.level_idc()));
    w.ue(0); // seq_parameter_set_id
    w.ue(1); // chroma_format_idc: 4:2:0
    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.bit(false); // qpprime_y_zero_transform_bypass_flag
    w.bit(false); // seq_scaling_matrix_present_flag
    w.ue(LOG2_MAX_FRAME_NUM - 4);
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.bit(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(p.width_mbs() - 1);
    w.ue(p.height_mbs() - 1);
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let (crop_r, crop_b) = (
        (p.width_mbs() * 16 - p.width) / 2,
        (p.height_mbs() * 16 - p.height) / 2,
    );
    let crop = crop_r != 0 || crop_b != 0;
    w.bit(crop);
    if crop {
        w.ue(0);
        w.ue(crop_r);
        w.ue(0);
        w.ue(crop_b);
    }
    w.bit(true); // vui_parameters_present_flag
    w.bit(false); // aspect_ratio_info_present_flag
    w.bit(false); // overscan_info_present_flag
    w.bit(true); // video_signal_type_present_flag
    w.bits(3, 5); // video_format: sin especificar
    w.bit(false); // video_full_range_flag: rango limitado
    w.bit(true); // colour_description_present_flag
    w.bits(8, 1); // colour_primaries: BT.709
    w.bits(8, 1); // transfer_characteristics: BT.709
    w.bits(8, 1); // matrix_coefficients: BT.709
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(true); // timing_info_present_flag
    w.bits(32, 1); // num_units_in_tick
    w.bits(32, p.fps.max(1) * 2); // time_scale
    w.bit(false); // fixed_frame_rate_flag (la pantalla quieta no manda frames)
    w.bit(false); // nal_hrd_parameters_present_flag
    w.bit(false); // vcl_hrd_parameters_present_flag
    w.bit(false); // pic_struct_present_flag
    w.bit(true); // bitstream_restriction_flag
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames: salida inmediata
    w.ue(1); // max_dec_frame_buffering
    w.trailing();
    nal(0x67, w) // nal_ref_idc 3, tipo 7
}

/// PPS completo (Annex-B).
pub fn pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(true); // entropy_coding_mode_flag: CABAC
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.bits(2, 0); // weighted_bipred_idc
    w.se(PIC_INIT_QP - 26); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    w.bit(true); // transform_8x8_mode_flag
    w.bit(false); // pic_scaling_matrix_present_flag
    w.se(0); // second_chroma_qp_index_offset
    w.trailing();
    nal(0x68, w) // nal_ref_idc 3, tipo 8
}

/// Lo que cambia en cada cabecera de *slice*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceParams {
    pub idr: bool,
    /// `frame_num` (módulo `2^LOG2_MAX_FRAME_NUM`).
    pub frame_num: u32,
    pub idr_pic_id: u32,
    /// QP del *slice* (se escribe como `slice_qp_delta`).
    pub qp: i32,
}

/// Cabecera de *slice* empaquetada: código de inicio, cabecera NAL y los
/// campos hasta `slice_beta_offset_div2`, **sin** alinear (el codificador
/// sigue escribiendo los datos del *slice* a continuación). Devuelve los
/// bytes y la longitud exacta en bits.
///
/// Solo se escapan los bytes completos: el último, a medias, lo completa el
/// codificador.
pub fn slice_header(s: &SliceParams) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.ue(0); // first_mb_in_slice
    w.ue(if s.idr { 7 } else { 5 }); // slice_type: I o P (todos los slices iguales)
    w.ue(0); // pic_parameter_set_id
    w.bits(LOG2_MAX_FRAME_NUM, s.frame_num % (1 << LOG2_MAX_FRAME_NUM));
    if s.idr {
        w.ue(s.idr_pic_id);
    }
    // pic_order_cnt_type 2: sin campos de POC.
    if !s.idr {
        w.bit(false); // num_ref_idx_active_override_flag
        w.bit(false); // ref_pic_list_modification_flag_l0
    }
    // dec_ref_pic_marking (nal_ref_idc != 0)
    if s.idr {
        w.bit(false); // no_output_of_prior_pics_flag
        w.bit(false); // long_term_reference_flag
    } else {
        w.bit(false); // adaptive_ref_pic_marking_mode_flag: ventana deslizante
    }
    if !s.idr {
        w.ue(0); // cabac_init_idc
    }
    w.se(s.qp - PIC_INIT_QP); // slice_qp_delta
    w.ue(0); // disable_deblocking_filter_idc
    w.se(0); // slice_alpha_c0_offset_div2
    w.se(0); // slice_beta_offset_div2
    let header_bits = w.bit_len();
    let used = header_bits % 8;
    let mut bytes = w.into_bytes();
    let partial = if used != 0 { bytes.pop() } else { None };
    let mut out = START_CODE.to_vec();
    out.push(if s.idr { 0x65 } else { 0x41 }); // nal_ref_idc 3 / 2, tipo 5 / 1
    out.extend_from_slice(&escape_rbsp(&bytes));
    let mut bit_len = out.len() * 8;
    if let Some(b) = partial {
        out.push(b);
        bit_len += used;
    }
    (out, bit_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::{parse_pps_start, parse_slice_start, parse_sps, BitReader, SliceType};

    #[test]
    fn exp_golomb_roundtrip() {
        let mut w = BitWriter::new();
        let ues = [0u32, 1, 2, 3, 7, 8, 255, 65_535, 1 << 20];
        let ses = [0i32, 1, -1, 2, -2, 25, -26, 1000];
        for v in ues {
            w.ue(v);
        }
        for v in ses {
            w.se(v);
        }
        w.trailing();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for v in ues {
            assert_eq!(r.ue().unwrap(), v);
        }
        for v in ses {
            assert_eq!(r.se().unwrap(), v);
        }
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(escape_rbsp(&[0, 0, 1]), vec![0, 0, 3, 1]);
        assert_eq!(escape_rbsp(&[0, 0, 0, 0]), vec![0, 0, 3, 0, 0]);
        assert_eq!(escape_rbsp(&[0, 0, 4]), vec![0, 0, 4]);
        assert_eq!(
            crate::h264::unescape_rbsp(&escape_rbsp(&[0, 0, 2, 0, 0, 3, 9])),
            vec![0, 0, 2, 0, 0, 3, 9]
        );
    }

    #[test]
    fn sps_describes_a_low_latency_stream() {
        for (w, h, fps) in [
            (1366, 768, 60),
            (1920, 1080, 60),
            (1280, 720, 30),
            (2560, 1440, 60),
        ] {
            let p = StreamParams {
                width: w,
                height: h,
                fps,
            };
            let nal = sps(&p);
            assert_eq!(&nal[..4], &START_CODE);
            let sps = parse_sps(&nal[4..]).unwrap();
            assert_eq!(sps.profile_idc, PROFILE_HIGH);
            assert_eq!((sps.width, sps.height), (w, h), "recorte de {w}x{h}");
            assert_eq!(sps.coded_width, p.width_mbs() * 16);
            assert_eq!(sps.pic_order_cnt_type, 2);
            assert_eq!(sps.max_num_ref_frames, 1);
            assert!(sps.frame_mbs_only);
            let vui = sps.vui.expect("VUI");
            assert_eq!(vui.max_num_reorder_frames, Some(0));
            assert_eq!(vui.max_dec_frame_buffering, Some(1));
            assert_eq!(vui.video_full_range, Some(false));
            assert_eq!(vui.colour_description, Some((1, 1, 1)));
            assert_eq!(vui.timing.map(|t| t.1), Some(fps * 2));
        }
    }

    #[test]
    fn levels_follow_table_a1() {
        let lvl = |width, height, fps| StreamParams { width, height, fps }.level_idc();
        assert_eq!(lvl(1366, 768, 60), 42);
        assert_eq!(lvl(1280, 720, 30), 31);
        assert_eq!(lvl(1920, 1080, 30), 40);
        assert_eq!(lvl(1920, 1080, 60), 42);
        assert_eq!(lvl(3840, 2160, 60), 52);
    }

    #[test]
    fn pps_uses_cabac() {
        let nal = pps();
        let p = parse_pps_start(&nal[4..]).unwrap();
        assert_eq!((p.pps_id, p.sps_id, p.cabac), (0, 0, true));
    }

    #[test]
    fn slice_headers_parse_and_report_exact_length() {
        for (idr, frame_num, qp) in [
            (true, 0, 30),
            (false, 1, 22),
            (false, 255, 40),
            (false, 256, 26),
        ] {
            let s = SliceParams {
                idr,
                frame_num,
                idr_pic_id: 3,
                qp,
            };
            let (bytes, bits) = slice_header(&s);
            assert!(bits <= bytes.len() * 8 && bits > (bytes.len() - 1) * 8);
            let start = parse_slice_start(&bytes[4..]).unwrap();
            assert_eq!(start.first_mb_in_slice, 0);
            assert_eq!(
                start.slice_type,
                if idr { SliceType::I } else { SliceType::P }
            );
            assert_eq!(bytes[4] & 0x1F, if idr { 5 } else { 1 });
            // frame_num tras slice_type y pps_id.
            let rbsp = crate::h264::unescape_rbsp(&bytes[5..]);
            let mut r = BitReader::new(&rbsp);
            r.ue().unwrap();
            r.ue().unwrap();
            r.ue().unwrap();
            assert_eq!(r.bits(LOG2_MAX_FRAME_NUM).unwrap(), frame_num % 256);
        }
    }
}
