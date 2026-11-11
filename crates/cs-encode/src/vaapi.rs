//! H.264 por hardware en Linux con VA-API (Intel Quick Sync, AMD VCN…).
//!
//! `libva.so.2` y `libva-drm.so.2` se cargan **en tiempo de ejecución**
//! (como OpenH264): si no están o la GPU no codifica H.264, el emisor sigue
//! con OpenH264 por CPU. VA-API, libva y los drivers de Intel son MIT/BSD.
//!
//! Por qué: con OpenH264 la codificación compite por la CPU con el resto del
//! equipo y, cargado, la pantalla extendida se retrasa y se congela. El
//! bloque de vídeo de la GPU codifica sin gastar CPU.
//!
//! Diseño (baja latencia, ADR-0005):
//! - punto de entrada `EncSliceLP` (VDEnc, función fija) si existe, si no
//!   `EncSlice`;
//! - QP constante por frame (`VA_RC_CQP`, el único que ofrece VDEnc sin el
//!   firmware HuC) con un control de tasa propio que sube o baja el QP según
//!   el tamaño de los frames;
//! - cabeceras SPS/PPS/*slice* escritas por nosotros
//!   ([`crate::h264_writer`]): mismo flujo en todos los drivers;
//! - un frame cada vez y síncrono: sin cola dentro del codificador.
//!
//! Las estructuras C se comprueban contra las cabeceras de libva 2.20
//! (tamaños y desplazamientos medidos con `offsetof`; ver los tests).
#![allow(unsafe_code)] // FFI con libva

use crate::h264_writer::{self, SliceParams, StreamParams, PIC_INIT_QP};
use crate::openh264_backend::I420Frame;
use crate::{
    EncodeControl, EncodeError, EncodedUnit, EncoderCaps, EncoderConfig, EncoderInfo, Result,
    VideoEncoder,
};
use libloading::Library;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

// --- Tipos y constantes de libva (va.h, va_enc_h264.h; libva 2.x) ---------

type VADisplay = *mut c_void;
type VAStatus = c_int;
type VAId = u32;

const VA_STATUS_SUCCESS: VAStatus = 0;
const VA_INVALID_ID: VAId = 0xFFFF_FFFF;

const VA_PROFILE_H264_HIGH: c_int = 7;
const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

const VA_RT_FORMAT_YUV420: u32 = 1;
const VA_RC_CQP: u32 = 0x10;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 1;
const VA_ENC_PACKED_HEADER_PICTURE: u32 = 2;
const VA_ENC_PACKED_HEADER_SLICE: u32 = 4;
const PACKED_HEADERS: u32 =
    VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE | VA_ENC_PACKED_HEADER_SLICE;

const VA_ENC_PACKED_HEADER_SEQUENCE_TYPE: u32 = 1;
const VA_ENC_PACKED_HEADER_PICTURE_TYPE: u32 = 2;
const VA_ENC_PACKED_HEADER_SLICE_TYPE: u32 = 3;

const VA_ENC_CODED_BUFFER_TYPE: c_int = 21;
const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: c_int = 22;
const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: c_int = 23;
const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: c_int = 24;
const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: c_int = 25;
const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: c_int = 26;

const VA_PROGRESSIVE: c_int = 1;
const VA_PICTURE_H264_INVALID: u32 = 1;
const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 8;
const VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK: u32 = 0x200;
/// `VA_FOURCC('N','V','1','2')`.
const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const VA_LSB_FIRST: u32 = 1;

/// Tipo de *slice* en los búferes VA (valores de H.264 `slice_type % 5`).
const VA_SLICE_P: u8 = 0;
const VA_SLICE_I: u8 = 2;

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy, Default)]
struct VAConfigAttrib {
    kind: c_int,
    value: u32,
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy)]
struct VAPictureH264 {
    picture_id: VAId,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}

impl VAPictureH264 {
    const INVALID: Self = Self {
        picture_id: VA_INVALID_ID,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy)]
struct VAEncSequenceParameterBufferH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy)]
struct VAEncPictureParameterBufferH264 {
    curr_pic: VAPictureH264,
    reference_frames: [VAPictureH264; 16],
    coded_buf: VAId,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy)]
struct VAEncSliceParameterBufferH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: VAId,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [VAPictureH264; 32],
    ref_pic_list1: [VAPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy)]
struct VAEncPackedHeaderParameterBuffer {
    kind: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
struct VACodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut c_void,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy, Default)]
struct VAImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[allow(dead_code)] // espejo de la estructura C: hay campos que solo lee el driver
#[derive(Clone, Copy, Default)]
struct VAImage {
    image_id: VAId,
    format: VAImageFormat,
    buf: VAId,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

/// Todo a cero es un valor válido para estas estructuras C (lo mismo que
/// hace `memset` en los ejemplos de libva).
fn zeroed<T: Copy>() -> T {
    // SAFETY: solo se usa con las estructuras `repr(C)` de este módulo, que
    // contienen enteros y arrays de enteros: el patrón de bits cero es válido.
    unsafe { std::mem::zeroed() }
}

// --- Carga dinámica ----------------------------------------------------------

macro_rules! va_fns {
    ($($name:ident: fn($($arg:ty),*) -> $ret:ty;)*) => {
        #[allow(non_snake_case)]
        struct Api {
            $($name: unsafe extern "C" fn($($arg),*) -> $ret,)*
            vaGetDisplayDRM: unsafe extern "C" fn(c_int) -> VADisplay,
            _va: Library,
            _drm: Library,
        }

        impl Api {
            fn load() -> std::result::Result<Self, String> {
                // SAFETY: cargar libva ejecuta sus constructores; es la
                // biblioteca del sistema que usan Firefox, mpv o GStreamer.
                let va = unsafe { Library::new("libva.so.2") }.map_err(|e| e.to_string())?;
                // SAFETY: ídem para libva-drm.
                let drm = unsafe { Library::new("libva-drm.so.2") }.map_err(|e| e.to_string())?;
                // SAFETY: las firmas coinciden con va.h / va_drm.h (libva 2.x)
                // y los punteros se guardan junto a las bibliotecas, que
                // viven tanto como `Api`.
                unsafe {
                    Ok(Self {
                        $($name: *va
                            .get::<unsafe extern "C" fn($($arg),*) -> $ret>(
                                concat!(stringify!($name), "\0").as_bytes(),
                            )
                            .map_err(|e| e.to_string())?,)*
                        vaGetDisplayDRM: *drm
                            .get::<unsafe extern "C" fn(c_int) -> VADisplay>(b"vaGetDisplayDRM\0")
                            .map_err(|e| e.to_string())?,
                        _va: va,
                        _drm: drm,
                    })
                }
            }
        }
    };
}

va_fns! {
    vaInitialize: fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
    vaTerminate: fn(VADisplay) -> VAStatus;
    vaErrorStr: fn(VAStatus) -> *const c_char;
    vaQueryVendorString: fn(VADisplay) -> *const c_char;
    vaMaxNumEntrypoints: fn(VADisplay) -> c_int;
    vaQueryConfigEntrypoints: fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus;
    vaGetConfigAttributes: fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus;
    vaCreateConfig: fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int, *mut VAId) -> VAStatus;
    vaDestroyConfig: fn(VADisplay, VAId) -> VAStatus;
    vaCreateSurfaces: fn(VADisplay, c_uint, c_uint, c_uint, *mut VAId, c_uint, *mut c_void, c_uint) -> VAStatus;
    vaDestroySurfaces: fn(VADisplay, *mut VAId, c_int) -> VAStatus;
    vaCreateContext: fn(VADisplay, VAId, c_int, c_int, c_int, *mut VAId, c_int, *mut VAId) -> VAStatus;
    vaDestroyContext: fn(VADisplay, VAId) -> VAStatus;
    vaCreateBuffer: fn(VADisplay, VAId, c_int, c_uint, c_uint, *mut c_void, *mut VAId) -> VAStatus;
    vaDestroyBuffer: fn(VADisplay, VAId) -> VAStatus;
    vaMapBuffer: fn(VADisplay, VAId, *mut *mut c_void) -> VAStatus;
    vaUnmapBuffer: fn(VADisplay, VAId) -> VAStatus;
    vaBeginPicture: fn(VADisplay, VAId, VAId) -> VAStatus;
    vaRenderPicture: fn(VADisplay, VAId, *mut VAId, c_int) -> VAStatus;
    vaEndPicture: fn(VADisplay, VAId) -> VAStatus;
    vaSyncSurface: fn(VADisplay, VAId) -> VAStatus;
    vaCreateImage: fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
    vaDestroyImage: fn(VADisplay, VAId) -> VAStatus;
    vaPutImage: fn(VADisplay, VAId, VAId, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> VAStatus;
}

/// Pantalla VA abierta sobre un nodo DRM; se cierra al soltarla.
struct Display {
    api: Arc<Api>,
    dpy: VADisplay,
    /// Mantiene abierto el nodo `/dev/dri/renderD*`.
    _node: std::fs::File,
    vendor: String,
}

// SAFETY: la pantalla VA solo se usa desde el hilo que posee el codificador
// (se mueve entre hilos, no se comparte); libva admite usarla desde otro hilo.
unsafe impl Send for Display {}

impl Display {
    fn check(&self, st: VAStatus, what: &str) -> Result<()> {
        if st == VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr devuelve una cadena estática terminada en NUL.
        let msg = unsafe { CStr::from_ptr((self.api.vaErrorStr)(st)) }.to_string_lossy();
        Err(EncodeError::Unsupported(format!(
            "VA-API {what}: {msg} ({st})"
        )))
    }

    fn open(api: Arc<Api>, path: &std::path::Path) -> std::result::Result<Self, String> {
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: descriptor válido mientras viva `node`, que se guarda.
        let dpy = unsafe { (api.vaGetDisplayDRM)(node.as_raw_fd()) };
        if dpy.is_null() {
            return Err(format!("{}: vaGetDisplayDRM falló", path.display()));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `dpy` recién obtenido; punteros a variables locales.
        let st = unsafe { (api.vaInitialize)(dpy, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS {
            // SAFETY: liberar la pantalla aunque no se inicializara.
            unsafe { (api.vaTerminate)(dpy) };
            return Err(format!("{}: vaInitialize falló ({st})", path.display()));
        }
        // SAFETY: cadena estática del driver mientras la pantalla viva.
        let vendor = unsafe {
            let p = (api.vaQueryVendorString)(dpy);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        Ok(Self {
            api,
            dpy,
            _node: node,
            vendor,
        })
    }

    /// Mejor punto de entrada para H.264 High con lo que necesitamos.
    fn h264_entrypoint(&self) -> Option<c_int> {
        // SAFETY: pantalla inicializada.
        let max = unsafe { (self.api.vaMaxNumEntrypoints)(self.dpy) }.max(1) as usize;
        let mut eps = vec![0 as c_int; max];
        let mut n = 0;
        // SAFETY: `eps` tiene `max` elementos, lo que libva pide.
        let st = unsafe {
            (self.api.vaQueryConfigEntrypoints)(
                self.dpy,
                VA_PROFILE_H264_HIGH,
                eps.as_mut_ptr(),
                &mut n,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        eps.truncate(n.clamp(0, max as c_int) as usize);
        [VA_ENTRYPOINT_ENC_SLICE_LP, VA_ENTRYPOINT_ENC_SLICE]
            .into_iter()
            .find(|ep| eps.contains(ep) && self.supports(*ep))
    }

    fn supports(&self, entrypoint: c_int) -> bool {
        let mut attrs = [
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: 0,
            },
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
        ];
        // SAFETY: array de 3 atributos y se pasa su longitud.
        let st = unsafe {
            (self.api.vaGetConfigAttributes)(
                self.dpy,
                VA_PROFILE_H264_HIGH,
                entrypoint,
                attrs.as_mut_ptr(),
                attrs.len() as c_int,
            )
        };
        let has = |a: &VAConfigAttrib, bits: u32| {
            a.value != VA_ATTRIB_NOT_SUPPORTED && a.value & bits == bits
        };
        st == VA_STATUS_SUCCESS
            && has(&attrs[0], VA_RT_FORMAT_YUV420)
            && has(&attrs[1], VA_RC_CQP)
            && has(&attrs[2], PACKED_HEADERS)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: pantalla inicializada; nada la usa ya.
        unsafe { (self.api.vaTerminate)(self.dpy) };
    }
}

/// Nodos DRM de render a probar, en orden.
fn render_nodes() -> Vec<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("CROSSSCREEN_VAAPI_DEVICE") {
        return vec![p.into()];
    }
    (128..=143)
        .map(|i| std::path::PathBuf::from(format!("/dev/dri/renderD{i}")))
        .filter(|p| p.exists())
        .collect()
}

/// Abre la primera GPU que codifique H.264 como lo necesitamos.
fn open_display() -> Result<(Display, c_int)> {
    let api = Arc::new(
        Api::load().map_err(|e| EncodeError::Unsupported(format!("libva no disponible: {e}")))?,
    );
    let mut tried = Vec::new();
    for node in render_nodes() {
        match Display::open(Arc::clone(&api), &node) {
            Ok(display) => match display.h264_entrypoint() {
                Some(ep) => return Ok((display, ep)),
                None => tried.push(format!(
                    "{} ({}): sin codificación H.264 adecuada",
                    node.display(),
                    display.vendor
                )),
            },
            Err(e) => tried.push(e),
        }
    }
    Err(EncodeError::Unsupported(if tried.is_empty() {
        "no hay nodos /dev/dri/renderD*".into()
    } else {
        tried.join("; ")
    }))
}

// --- Codificador -------------------------------------------------------------

/// Límites del QP del control de tasa propio.
const QP_MIN: i32 = 18;
const QP_MAX: i32 = 42;
const QP_START: i32 = 26;

/// Codificador H.264 por VA-API.
pub struct VaapiEncoder {
    display: Display,
    entrypoint: c_int,
    config: VAId,
    context: VAId,
    /// Superficie de entrada + dos reconstruidas (actual y referencia).
    surfaces: [VAId; 3],
    coded_buf: VAId,
    image: VAImage,
    params: StreamParams,
    bitrate_kbps: u32,
    caps: EncoderCaps,
    /// Superficie reconstruida que sirve de referencia (índice 1 o 2).
    reference: Option<usize>,
    ref_frame_num: u32,
    ref_poc: i32,
    frame_num: u32,
    frames_since_idr: u32,
    idr_pic_id: u32,
    qp: i32,
    /// Media móvil del tamaño de los P-frames (bytes).
    p_bytes_avg: f64,
    force_idr: bool,
    next_pts_us: u64,
    frame_duration_us: u64,
}

impl std::fmt::Debug for VaapiEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiEncoder")
            .field("vendor", &self.display.vendor)
            .field("entrypoint", &self.entrypoint)
            .field("params", &self.params)
            .field("qp", &self.qp)
            .finish_non_exhaustive()
    }
}

impl VaapiEncoder {
    /// Abre la GPU y prepara el codificador para `cfg` (solo H.264).
    pub fn new(cfg: &EncoderConfig) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width % 2 != 0 || cfg.height % 2 != 0 {
            return Err(EncodeError::BadInput(format!(
                "dimensiones no válidas para H.264: {}x{}",
                cfg.width, cfg.height
            )));
        }
        let (display, entrypoint) = open_display()?;
        let fps = (cfg.fps_num / cfg.fps_den.max(1)).max(1);
        let params = StreamParams {
            width: cfg.width,
            height: cfg.height,
            fps,
        };
        let (aw, ah) = (params.width_mbs() * 16, params.height_mbs() * 16);
        let api = &display.api;
        let dpy = display.dpy;

        let mut attrs = [
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: VA_RC_CQP,
            },
            VAConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: PACKED_HEADERS,
            },
        ];
        let mut config = VA_INVALID_ID;
        // SAFETY: atributos locales con su longitud; `config` es de salida.
        let st = unsafe {
            (api.vaCreateConfig)(
                dpy,
                VA_PROFILE_H264_HIGH,
                entrypoint,
                attrs.as_mut_ptr(),
                attrs.len() as c_int,
                &mut config,
            )
        };
        display.check(st, "vaCreateConfig")?;

        // A partir de aquí, `enc` libera lo creado si algo falla.
        let mut enc = Self {
            display,
            entrypoint,
            config,
            context: VA_INVALID_ID,
            surfaces: [VA_INVALID_ID; 3],
            coded_buf: VA_INVALID_ID,
            image: VAImage {
                image_id: VA_INVALID_ID,
                ..VAImage::default()
            },
            params,
            bitrate_kbps: cfg.bitrate_kbps,
            caps: EncoderCaps {
                info: EncoderInfo {
                    friendly_name: String::new(),
                    vendor_id: None,
                    hardware: true,
                    adapter_luid: None,
                },
                async_mft: false,
                d3d11_aware: false,
                provides_output_samples: false,
                sequence_header_in_type: false,
                properties: Vec::new(),
            },
            reference: None,
            ref_frame_num: 0,
            ref_poc: 0,
            frame_num: 0,
            frames_since_idr: 0,
            idr_pic_id: 0,
            qp: QP_START,
            p_bytes_avg: 0.0,
            force_idr: true,
            next_pts_us: 0,
            frame_duration_us: cfg.frame_duration_us(),
        };
        enc.caps.info.friendly_name = format!(
            "VA-API {} ({})",
            enc.display.vendor,
            if entrypoint == VA_ENTRYPOINT_ENC_SLICE_LP {
                "EncSliceLP"
            } else {
                "EncSlice"
            }
        );
        let api = &enc.display.api;

        // SAFETY: array de 3 superficies de salida.
        let st = unsafe {
            (api.vaCreateSurfaces)(
                dpy,
                VA_RT_FORMAT_YUV420,
                aw,
                ah,
                enc.surfaces.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                0,
            )
        };
        enc.display.check(st, "vaCreateSurfaces")?;
        let mut targets = enc.surfaces;
        // SAFETY: superficies recién creadas; `context` de salida.
        let st = unsafe {
            (api.vaCreateContext)(
                dpy,
                enc.config,
                aw as c_int,
                ah as c_int,
                VA_PROGRESSIVE,
                targets.as_mut_ptr(),
                3,
                &mut enc.context,
            )
        };
        enc.display.check(st, "vaCreateContext")?;
        // Peor caso holgado: un IDR de calidad alta no supera 1,5 bytes/píxel.
        let coded_size = aw * ah * 3 / 2 + (1 << 20);
        // SAFETY: búfer de salida sin datos iniciales.
        let st = unsafe {
            (api.vaCreateBuffer)(
                dpy,
                enc.context,
                VA_ENC_CODED_BUFFER_TYPE,
                coded_size,
                1,
                std::ptr::null_mut(),
                &mut enc.coded_buf,
            )
        };
        enc.display.check(st, "búfer de salida")?;
        let mut format = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            byte_order: VA_LSB_FIRST,
            bits_per_pixel: 12,
            ..VAImageFormat::default()
        };
        // SAFETY: formato local; `image` de salida.
        let st = unsafe {
            (api.vaCreateImage)(dpy, &mut format, aw as c_int, ah as c_int, &mut enc.image)
        };
        enc.display.check(st, "vaCreateImage NV12")?;
        if enc.image.num_planes < 2
            || enc.image.pitches[0] < aw
            || enc.image.pitches[1] < aw
            || u64::from(enc.image.offsets[1]) + u64::from(enc.image.pitches[1]) * u64::from(ah / 2)
                > u64::from(enc.image.data_size)
            || u64::from(enc.image.pitches[0]) * u64::from(ah) > u64::from(enc.image.offsets[1])
        {
            return Err(EncodeError::Unsupported(
                "la imagen NV12 de VA-API tiene una disposición inesperada".into(),
            ));
        }
        Ok(enc)
    }

    fn api(&self) -> &Api {
        &self.display.api
    }

    /// Copia el frame I420 a la superficie de entrada (NV12).
    fn upload(&mut self, frame: &I420Frame) -> Result<()> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let img = self.image;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: búfer de la imagen creada en `new`.
        let st = unsafe { (self.api().vaMapBuffer)(self.display.dpy, img.buf, &mut ptr) };
        self.display.check(st, "vaMapBuffer imagen")?;
        if ptr.is_null() {
            return Err(EncodeError::Unsupported("imagen VA sin memoria".into()));
        }
        {
            // SAFETY: libva garantiza `data_size` bytes mapeados en `ptr`
            // hasta `vaUnmapBuffer`; nadie más accede mientras tanto.
            let dst =
                unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), img.data_size as usize) };
            let (yp, uvp) = (img.pitches[0] as usize, img.pitches[1] as usize);
            let uv_off = img.offsets[1] as usize;
            let (ys, us, vs) = frame.strides;
            for row in 0..h {
                let src = &frame.y[row * ys..row * ys + w];
                let start = img.offsets[0] as usize + row * yp;
                dst[start..start + w].copy_from_slice(src);
            }
            let cw = w / 2;
            for row in 0..h / 2 {
                let u = &frame.u[row * us..row * us + cw];
                let v = &frame.v[row * vs..row * vs + cw];
                let line = &mut dst[uv_off + row * uvp..uv_off + row * uvp + 2 * cw];
                for ((pair, &cb), &cr) in line.chunks_exact_mut(2).zip(u).zip(v) {
                    pair[0] = cb;
                    pair[1] = cr;
                }
            }
        }
        // SAFETY: mapeado arriba.
        let st = unsafe { (self.api().vaUnmapBuffer)(self.display.dpy, img.buf) };
        self.display.check(st, "vaUnmapBuffer imagen")?;
        let (aw, ah) = (u32::from(img.width), u32::from(img.height));
        // SAFETY: imagen y superficie del mismo tamaño, creadas en `new`.
        let st = unsafe {
            (self.api().vaPutImage)(
                self.display.dpy,
                self.surfaces[0],
                img.image_id,
                0,
                0,
                aw,
                ah,
                0,
                0,
                aw,
                ah,
            )
        };
        self.display.check(st, "vaPutImage")
    }

    /// Crea un búfer de parámetros con una copia de `value`.
    fn buffer<T: Copy>(&self, kind: c_int, value: &T) -> Result<VAId> {
        self.buffer_bytes(kind, std::mem::size_of::<T>(), (value as *const T).cast())
    }

    fn buffer_bytes(&self, kind: c_int, size: usize, data: *const c_void) -> Result<VAId> {
        let mut id = VA_INVALID_ID;
        // SAFETY: libva copia `size` bytes desde `data`, que apunta a un
        // valor o slice vivo de ese tamaño.
        let st = unsafe {
            (self.api().vaCreateBuffer)(
                self.display.dpy,
                self.context,
                kind,
                size as c_uint,
                1,
                data.cast_mut(),
                &mut id,
            )
        };
        self.display.check(st, "vaCreateBuffer")?;
        Ok(id)
    }

    fn packed(&self, kind: u32, data: &[u8], bit_len: usize, out: &mut Vec<VAId>) -> Result<()> {
        let param = VAEncPackedHeaderParameterBuffer {
            kind,
            bit_length: bit_len as u32,
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        out.push(self.buffer(VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE, &param)?);
        out.push(self.buffer_bytes(
            VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
            data.len(),
            data.as_ptr().cast(),
        )?);
        Ok(())
    }

    fn sequence_params(&self) -> VAEncSequenceParameterBufferH264 {
        let p = &self.params;
        let mut s: VAEncSequenceParameterBufferH264 = zeroed();
        s.level_idc = p.level_idc();
        s.intra_period = p.fps.saturating_mul(10);
        s.intra_idr_period = s.intra_period;
        s.ip_period = 1;
        s.bits_per_second = self.bitrate_kbps.saturating_mul(1000);
        s.max_num_ref_frames = 1;
        s.picture_width_in_mbs = p.width_mbs() as u16;
        s.picture_height_in_mbs = p.height_mbs() as u16;
        // chroma_format_idc=1 | frame_mbs_only | direct_8x8_inference |
        // log2_max_frame_num_minus4 | pic_order_cnt_type=2
        s.seq_fields =
            1 | (1 << 2) | (1 << 5) | ((h264_writer::LOG2_MAX_FRAME_NUM - 4) << 6) | (2 << 10);
        let (crop_r, crop_b) = (
            (p.width_mbs() * 16 - p.width) / 2,
            (p.height_mbs() * 16 - p.height) / 2,
        );
        if crop_r != 0 || crop_b != 0 {
            s.frame_cropping_flag = 1;
            s.frame_crop_right_offset = crop_r;
            s.frame_crop_bottom_offset = crop_b;
        }
        s.vui_parameters_present_flag = 1;
        // timing_info | bitstream_restriction | mv 16/16 | mv sobre bordes
        s.vui_fields = (1 << 1) | (1 << 2) | (16 << 3) | (16 << 8) | (1 << 15);
        s.num_units_in_tick = 1;
        s.time_scale = p.fps * 2;
        s
    }

    fn encode_frame(&mut self, frame: &I420Frame, idr: bool) -> Result<(Vec<u8>, bool)> {
        self.upload(frame)?;
        if idr {
            self.frame_num = 0;
            self.frames_since_idr = 0;
            self.reference = None;
            self.idr_pic_id = (self.idr_pic_id + 1) % 65_536;
        }
        let current = match self.reference {
            Some(1) => 2,
            _ => 1,
        };
        let poc = (self.frames_since_idr.wrapping_mul(2)) as i32;
        let qp = if idr {
            (self.qp + 2).min(QP_MAX)
        } else {
            self.qp
        };

        let mut buffers = Vec::with_capacity(9);
        let result = self.render(idr, current, poc, qp, &mut buffers);
        for id in buffers {
            // SAFETY: búferes creados en `render`; ya no se usan.
            unsafe { (self.api().vaDestroyBuffer)(self.display.dpy, id) };
        }
        result?;

        // Esperar al codificador y leer la salida.
        // SAFETY: superficie de entrada del frame en curso.
        let st = unsafe { (self.api().vaSyncSurface)(self.display.dpy, self.surfaces[0]) };
        self.display.check(st, "vaSyncSurface")?;
        let mut seg_ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: búfer de salida creado en `new`.
        let st =
            unsafe { (self.api().vaMapBuffer)(self.display.dpy, self.coded_buf, &mut seg_ptr) };
        self.display.check(st, "vaMapBuffer salida")?;
        let mut data = Vec::new();
        let mut overflow = false;
        let mut seg = seg_ptr.cast::<VACodedBufferSegment>();
        while !seg.is_null() {
            // SAFETY: lista de segmentos válida mientras el búfer esté mapeado.
            let s = unsafe { &*seg };
            overflow |= s.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
            if !s.buf.is_null() && s.size > 0 {
                // SAFETY: `size` bytes en `buf`, según libva.
                data.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf.cast::<u8>(), s.size as usize)
                });
            }
            seg = s.next.cast();
        }
        // SAFETY: mapeado arriba.
        let st = unsafe { (self.api().vaUnmapBuffer)(self.display.dpy, self.coded_buf) };
        self.display.check(st, "vaUnmapBuffer salida")?;
        if overflow {
            // Frame truncado: no sirve. Subir el QP y repetir con un IDR.
            self.qp = (self.qp + 6).min(QP_MAX);
            self.reference = None;
            return Err(EncodeError::Unsupported(
                "el frame no cupo en el búfer de salida".into(),
            ));
        }
        self.reference = Some(current);
        self.ref_frame_num = self.frame_num;
        self.ref_poc = poc;
        self.frame_num = (self.frame_num + 1) % (1 << h264_writer::LOG2_MAX_FRAME_NUM);
        self.frames_since_idr = self.frames_since_idr.wrapping_add(1);
        if !idr {
            self.rate_control(data.len());
        }
        Ok((data, idr))
    }

    fn render(
        &self,
        idr: bool,
        current: usize,
        poc: i32,
        qp: i32,
        buffers: &mut Vec<VAId>,
    ) -> Result<()> {
        if idr {
            let seq = self.sequence_params();
            buffers.push(self.buffer(VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &seq)?);
            let sps = h264_writer::sps(&self.params);
            self.packed(
                VA_ENC_PACKED_HEADER_SEQUENCE_TYPE,
                &sps,
                sps.len() * 8,
                buffers,
            )?;
            let pps = h264_writer::pps();
            self.packed(
                VA_ENC_PACKED_HEADER_PICTURE_TYPE,
                &pps,
                pps.len() * 8,
                buffers,
            )?;
        }
        let reference = (!idr)
            .then_some(self.reference)
            .flatten()
            .map(|r| VAPictureH264 {
                picture_id: self.surfaces[r],
                frame_idx: self.ref_frame_num,
                flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                top_field_order_cnt: self.ref_poc,
                bottom_field_order_cnt: self.ref_poc,
                va_reserved: [0; 4],
            });

        let mut pic: VAEncPictureParameterBufferH264 = zeroed();
        pic.curr_pic = VAPictureH264 {
            picture_id: self.surfaces[current],
            frame_idx: self.frame_num,
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            va_reserved: [0; 4],
        };
        pic.reference_frames = [VAPictureH264::INVALID; 16];
        if let Some(r) = reference {
            pic.reference_frames[0] = r;
        }
        pic.coded_buf = self.coded_buf;
        pic.frame_num = self.frame_num as u16;
        pic.pic_init_qp = PIC_INIT_QP as u8;
        // idr | reference_pic_flag=1 | CABAC | transform_8x8 | deblocking_control
        pic.pic_fields = u32::from(idr) | (1 << 1) | (1 << 3) | (1 << 8) | (1 << 9);
        buffers.push(self.buffer(VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &pic)?);

        let (header, bits) = h264_writer::slice_header(&SliceParams {
            idr,
            frame_num: self.frame_num,
            idr_pic_id: self.idr_pic_id,
            qp,
        });
        self.packed(VA_ENC_PACKED_HEADER_SLICE_TYPE, &header, bits, buffers)?;

        let mut slice: VAEncSliceParameterBufferH264 = zeroed();
        slice.num_macroblocks = self.params.width_mbs() * self.params.height_mbs();
        slice.macroblock_info = VA_INVALID_ID;
        slice.slice_type = if idr { VA_SLICE_I } else { VA_SLICE_P };
        slice.idr_pic_id = self.idr_pic_id as u16;
        slice.direct_spatial_mv_pred_flag = 1;
        slice.ref_pic_list0 = [VAPictureH264::INVALID; 32];
        slice.ref_pic_list1 = [VAPictureH264::INVALID; 32];
        if let Some(r) = reference {
            slice.ref_pic_list0[0] = r;
        }
        slice.slice_qp_delta = (qp - PIC_INIT_QP) as i8;
        buffers.push(self.buffer(VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &slice)?);

        let api = self.api();
        let dpy = self.display.dpy;
        // SAFETY: contexto y superficie de entrada creados en `new`.
        let st = unsafe { (api.vaBeginPicture)(dpy, self.context, self.surfaces[0]) };
        self.display.check(st, "vaBeginPicture")?;
        let mut ids = buffers.clone();
        // SAFETY: lista de búferes válidos del mismo contexto.
        let st = unsafe {
            (api.vaRenderPicture)(dpy, self.context, ids.as_mut_ptr(), ids.len() as c_int)
        };
        let render = self.display.check(st, "vaRenderPicture");
        // SAFETY: cerrar siempre el frame empezado.
        let st = unsafe { (api.vaEndPicture)(dpy, self.context) };
        render?;
        self.display.check(st, "vaEndPicture")
    }

    /// Control de tasa propio sobre QP constante: compara la media de los
    /// P-frames con el presupuesto por frame y mueve el QP un paso.
    fn rate_control(&mut self, bytes: usize) {
        let budget = f64::from(self.bitrate_kbps) * 1000.0 / 8.0 / f64::from(self.params.fps);
        let b = bytes as f64;
        self.p_bytes_avg = if self.p_bytes_avg == 0.0 {
            b
        } else {
            self.p_bytes_avg * 0.8 + b * 0.2
        };
        if self.p_bytes_avg > budget * 1.25 || b > budget * 4.0 {
            self.qp = (self.qp + 1).min(QP_MAX);
        } else if self.p_bytes_avg < budget * 0.5 {
            self.qp = (self.qp - 1).max(QP_MIN);
        }
    }

    /// Cambia la tasa objetivo del control de tasa propio (p. ej. cuando el
    /// emisor detecta la red atascada). Surte efecto en los frames siguientes.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) {
        self.bitrate_kbps = kbps.max(1);
    }

    /// QP actual (diagnóstico).
    pub fn qp(&self) -> i32 {
        self.qp
    }
}

impl VideoEncoder for VaapiEncoder {
    type Frame = I420Frame;

    fn encode(&mut self, frame: &Self::Frame, ctl: EncodeControl) -> Result<Vec<EncodedUnit>> {
        if (frame.width, frame.height) != (self.params.width, self.params.height) {
            return Err(EncodeError::BadInput(format!(
                "frame de {}x{} en un codificador de {}x{}",
                frame.width, frame.height, self.params.width, self.params.height
            )));
        }
        let idr = ctl.force_idr || self.force_idr || self.reference.is_none();
        let start = Instant::now();
        let pts_us = self.next_pts_us;
        self.next_pts_us = self.next_pts_us.saturating_add(self.frame_duration_us);
        match self.encode_frame(frame, idr) {
            Ok((data, keyframe)) => {
                self.force_idr = false;
                Ok(vec![EncodedUnit {
                    data,
                    keyframe,
                    pts_us,
                    encode_time: start.elapsed(),
                    hw_time: Duration::ZERO,
                }])
            }
            Err(e) => {
                self.force_idr = true;
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedUnit>> {
        Ok(Vec::new())
    }

    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let api = &self.display.api;
        let dpy = self.display.dpy;
        // SAFETY: se destruye solo lo que se creó (ids distintos de
        // VA_INVALID_ID), en orden inverso, antes de `vaTerminate` (Drop de
        // `Display`, que va después).
        unsafe {
            if self.image.image_id != VA_INVALID_ID {
                (api.vaDestroyImage)(dpy, self.image.image_id);
            }
            if self.coded_buf != VA_INVALID_ID {
                (api.vaDestroyBuffer)(dpy, self.coded_buf);
            }
            if self.context != VA_INVALID_ID {
                (api.vaDestroyContext)(dpy, self.context);
            }
            if self.surfaces[0] != VA_INVALID_ID {
                (api.vaDestroySurfaces)(dpy, self.surfaces.as_mut_ptr(), 3);
            }
            if self.config != VA_INVALID_ID {
                (api.vaDestroyConfig)(dpy, self.config);
            }
        }
    }
}

/// ¿Hay una GPU que codifique H.264 por VA-API? Devuelve su descripción.
pub fn probe() -> Result<String> {
    let (display, ep) = open_display()?;
    Ok(format!(
        "{} ({})",
        display.vendor,
        if ep == VA_ENTRYPOINT_ENC_SLICE_LP {
            "EncSliceLP"
        } else {
            "EncSlice"
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Tamaños y desplazamientos medidos con `sizeof`/`offsetof` contra las
    /// cabeceras de libva 2.20 (x86_64). Si cambian, las estructuras de
    /// arriba están mal y el driver leería basura.
    #[test]
    fn struct_layouts_match_libva_headers() {
        assert_eq!(size_of::<VAPictureH264>(), 36);
        assert_eq!(offset_of!(VAPictureH264, top_field_order_cnt), 12);

        type S = VAEncSequenceParameterBufferH264;
        assert_eq!(size_of::<S>(), 1132);
        assert_eq!(offset_of!(S, intra_period), 4);
        assert_eq!(offset_of!(S, bits_per_second), 16);
        assert_eq!(offset_of!(S, picture_width_in_mbs), 24);
        assert_eq!(offset_of!(S, seq_fields), 28);
        assert_eq!(offset_of!(S, offset_for_non_ref_pic), 36);
        assert_eq!(offset_of!(S, offset_for_ref_frame), 44);
        assert_eq!(offset_of!(S, frame_cropping_flag), 1068);
        assert_eq!(offset_of!(S, frame_crop_left_offset), 1072);
        assert_eq!(offset_of!(S, frame_crop_bottom_offset), 1084);
        assert_eq!(offset_of!(S, vui_parameters_present_flag), 1088);
        assert_eq!(offset_of!(S, vui_fields), 1092);
        assert_eq!(offset_of!(S, sar_width), 1100);
        assert_eq!(offset_of!(S, time_scale), 1112);
        assert_eq!(offset_of!(S, va_reserved), 1116);

        type P = VAEncPictureParameterBufferH264;
        assert_eq!(size_of::<P>(), 648);
        assert_eq!(offset_of!(P, reference_frames), 36);
        assert_eq!(offset_of!(P, coded_buf), 612);
        assert_eq!(offset_of!(P, last_picture), 618);
        assert_eq!(offset_of!(P, frame_num), 620);
        assert_eq!(offset_of!(P, pic_init_qp), 622);
        assert_eq!(offset_of!(P, chroma_qp_index_offset), 625);
        assert_eq!(offset_of!(P, pic_fields), 628);
        assert_eq!(offset_of!(P, va_reserved), 632);

        type L = VAEncSliceParameterBufferH264;
        assert_eq!(size_of::<L>(), 3140);
        assert_eq!(offset_of!(L, macroblock_info), 8);
        assert_eq!(offset_of!(L, slice_type), 12);
        assert_eq!(offset_of!(L, idr_pic_id), 14);
        assert_eq!(offset_of!(L, delta_pic_order_cnt_bottom), 20);
        assert_eq!(offset_of!(L, direct_spatial_mv_pred_flag), 32);
        assert_eq!(offset_of!(L, ref_pic_list0), 36);
        assert_eq!(offset_of!(L, ref_pic_list1), 1188);
        assert_eq!(offset_of!(L, luma_log2_weight_denom), 2340);
        assert_eq!(offset_of!(L, luma_weight_l0), 2344);
        assert_eq!(offset_of!(L, chroma_weight_l0_flag), 2472);
        assert_eq!(offset_of!(L, chroma_weight_l0), 2474);
        assert_eq!(offset_of!(L, luma_weight_l1_flag), 2730);
        assert_eq!(offset_of!(L, chroma_weight_l1_flag), 2860);
        assert_eq!(offset_of!(L, chroma_offset_l1), 2990);
        assert_eq!(offset_of!(L, cabac_init_idc), 3118);
        assert_eq!(offset_of!(L, slice_qp_delta), 3119);
        assert_eq!(offset_of!(L, disable_deblocking_filter_idc), 3120);
        assert_eq!(offset_of!(L, slice_beta_offset_div2), 3122);
        assert_eq!(offset_of!(L, va_reserved), 3124);

        assert_eq!(size_of::<VAEncPackedHeaderParameterBuffer>(), 28);
        assert_eq!(
            offset_of!(VAEncPackedHeaderParameterBuffer, has_emulation_bytes),
            8
        );
        assert_eq!(size_of::<VACodedBufferSegment>(), 48);
        assert_eq!(offset_of!(VACodedBufferSegment, buf), 16);
        assert_eq!(offset_of!(VACodedBufferSegment, next), 24);
        assert_eq!(size_of::<VAImageFormat>(), 48);
        assert_eq!(size_of::<VAImage>(), 120);
        assert_eq!(offset_of!(VAImage, buf), 52);
        assert_eq!(offset_of!(VAImage, data_size), 60);
        assert_eq!(offset_of!(VAImage, pitches), 68);
        assert_eq!(offset_of!(VAImage, offsets), 80);
        assert_eq!(offset_of!(VAImage, component_order), 100);
        assert_eq!(size_of::<VAConfigAttrib>(), 8);
        assert_eq!(VA_FOURCC_NV12, 842_094_158);
    }
}
