//! Codificador H.264 por hardware con Media Foundation (Windows).
//!
//! Flujo:
//! 1. `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, HARDWARE | SORTANDFILTER)` con
//!    entrada NV12 y salida H.264; se elige el MFT del adaptador del
//!    dispositivo D3D11 (por LUID, si no por fabricante).
//! 2. Se desbloquea el modelo asíncrono (`MF_TRANSFORM_ASYNC_UNLOCK`) y se le
//!    pasa un `IMFDXGIDeviceManager` con el **mismo** dispositivo D3D11.
//! 3. Se configura por `ICodecAPI` (baja latencia, CBR, GOP, sin B…) y se
//!    registra qué propiedades acepta o rechaza el MFT.
//! 4. Cada frame BGRA se convierte a NV12 en la GPU ([`convert::Nv12Converter`])
//!    y se entrega con `MFCreateDXGISurfaceBuffer`, sin copias a CPU.
//! 5. Los eventos `METransformNeedInput` / `METransformHaveOutput` se leen en
//!    un hilo dedicado (`GetEvent` bloqueante) y se reenvían por un canal.
#![allow(unsafe_code)] // FFI Media Foundation / D3D11; cada bloque unsafe está comentado.

pub mod convert;

use crate::bitstream::{self, VideoCodec};
use crate::{
    EncodeControl, EncodeError, EncodedUnit, EncoderCaps, EncoderConfig, EncoderInfo, H264Profile,
    PropertyOutcome, PropertyReport, Result, VideoEncoder,
};
use convert::{enable_multithread_protection, Nv12Converter};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Foundation::{FALSE, HMODULE};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};

pub use convert::Nv12Converter as Converter;

/// Convierte un error de `windows` en [`EncodeError::Os`].
pub(crate) fn hr_err(call: &'static str, e: windows::core::Error) -> EncodeError {
    EncodeError::Os {
        call,
        hresult: e.code().0 as u32,
    }
}

/// Frame de entrada: textura BGRA8 del mismo `ID3D11Device` que el codificador.
#[derive(Debug, Clone)]
pub struct InputFrame {
    pub texture: ID3D11Texture2D,
    /// Subrecurso (índice de *array slice*); 0 para texturas simples.
    pub subresource: u32,
    /// Marca de tiempo de presentación en µs (monótona creciente).
    pub pts_us: u64,
}

/// Dispositivo D3D11 creado para captura + codificación.
#[derive(Debug, Clone)]
pub struct D3D11Setup {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub luid: u64,
}

fn luid_to_u64(l: windows::Win32::Foundation::LUID) -> u64 {
    ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64
}

fn wide_to_string(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

/// Crea un dispositivo D3D11 con soporte de vídeo y BGRA en el primer
/// adaptador hardware del fabricante pedido (p. ej. `0x8086` Intel), o en el
/// primero hardware si `prefer_vendor` es `None` o no existe.
pub fn create_d3d11_device(prefer_vendor: Option<u32>) -> Result<D3D11Setup> {
    // SAFETY: creación de la fábrica DXGI sin parámetros.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| hr_err("CreateDXGIFactory1", e))?;
    let mut adapters = Vec::new();
    for i in 0.. {
        // SAFETY: enumeración con índice; termina con DXGI_ERROR_NOT_FOUND.
        let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: adaptador válido.
        let Ok(desc) = (unsafe { a.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        adapters.push((a, desc));
    }
    let pick = prefer_vendor
        .and_then(|v| adapters.iter().position(|(_, d)| d.VendorId == v))
        .unwrap_or(0);
    let (adapter, desc) = adapters
        .get(pick)
        .cloned()
        .ok_or_else(|| EncodeError::NoEncoder("no hay adaptadores DXGI hardware".into()))?;

    let mut device = None;
    let mut context = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    // SAFETY: punteros de salida a Options locales; adaptador válido.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| hr_err("D3D11CreateDevice", e))?;
    Ok(D3D11Setup {
        device: device.ok_or(EncodeError::NoEncoder("D3D11CreateDevice nulo".into()))?,
        context: context.ok_or(EncodeError::NoEncoder("contexto nulo".into()))?,
        adapter_name: wide_to_string(&desc.Description),
        vendor_id: desc.VendorId,
        luid: luid_to_u64(desc.AdapterLuid),
    })
}

fn device_luid(device: &ID3D11Device) -> Option<u64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    // SAFETY: llamadas COM sobre interfaces válidas.
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    // SAFETY: ídem.
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(luid_to_u64(desc.AdapterLuid))
}

/// Inicializa COM (MTA) y Media Foundation para el hilo actual.
fn mf_startup() -> Result<()> {
    // SAFETY: inicialización COM; si el hilo ya tiene otro modelo devuelve
    // RPC_E_CHANGED_MODE, que se tolera (MF funciona igual en el MTA implícito).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: MFStartup es contado por referencia; se empareja con MFShutdown.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| hr_err("MFStartup", e))
}

fn get_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: GetAllocatedString reserva con CoTaskMemAlloc; se libera abajo.
    unsafe { attrs.GetAllocatedString(key, &mut p, &mut len) }.ok()?;
    // SAFETY: `p` apunta a `len` u16 válidos terminados en 0.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: liberar la memoria reservada por MF.
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// Subtipo de Media Foundation de cada códec.
pub fn codec_subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

/// Enumera los MFT codificadores H.264 por hardware (entrada NV12).
pub fn enumerate_hw_h264_encoders() -> Result<Vec<(EncoderInfo, IMFActivate)>> {
    enumerate_hw_encoders(VideoCodec::H264)
}

/// Códecs que este equipo codifica por hardware sobre `device`.
///
/// No basta con que el MFT esté registrado: algunos drivers registran
/// codificadores que el hardware no tiene (p. ej. AV1 en Intel de 12.ª
/// generación, que falla al empezar el *streaming*). Se crea de verdad un
/// codificador pequeño de cada códec.
pub fn hw_encoder_codecs(device: &ID3D11Device) -> Vec<VideoCodec> {
    [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
        .into_iter()
        .filter(|c| {
            if !enumerate_hw_encoders(*c).is_ok_and(|l| !l.is_empty()) {
                return false;
            }
            let mut cfg = EncoderConfig::desktop(640, 360, 30, 1_000);
            cfg.codec = *c;
            cfg.raise_timer_resolution = false;
            match MfH264Encoder::new(device, cfg) {
                Ok(_) => true,
                Err(e) => {
                    tracing::info!("codificador {c:?} registrado pero no utilizable: {e}");
                    false
                }
            }
        })
        .collect()
}

/// Enumera los MFT codificadores por hardware de `codec` (entrada NV12).
pub fn enumerate_hw_encoders(codec: VideoCodec) -> Result<Vec<(EncoderInfo, IMFActivate)>> {
    mf_startup()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec_subtype(codec),
    };
    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: punteros de salida válidos; MF reserva el array con CoTaskMemAlloc.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut arr,
            &mut count,
        )
    }
    .map_err(|e| hr_err("MFTEnumEx", e))?;
    let mut out = Vec::new();
    for i in 0..count as usize {
        // SAFETY: `arr` tiene `count` elementos; `read` toma la propiedad de
        // cada referencia, que se libera al soltar el Option.
        let act = unsafe { std::ptr::read(arr.add(i)) };
        let Some(act) = act else { continue };
        let attrs: &IMFAttributes = &act;
        let info = EncoderInfo {
            friendly_name: get_string(attrs, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "(sin nombre)".into()),
            vendor_id: get_string(attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
            hardware: true,
            // SAFETY: lectura de atributo UINT64.
            adapter_luid: unsafe { attrs.GetUINT64(&MFT_ENUM_ADAPTER_LUID) }.ok(),
        };
        out.push((info, act));
    }
    if !arr.is_null() {
        // SAFETY: liberar el array reservado por MFTEnumEx (los elementos ya se movieron).
        unsafe { CoTaskMemFree(Some(arr as *const _)) };
    }
    // SAFETY: empareja el MFStartup de esta función.
    let _ = unsafe { MFShutdown() };
    Ok(out)
}

/// Parámetros de secuencia del `MF_MT_MPEG_SEQUENCE_HEADER` del tipo de
/// salida. En AV1 puede venir como `av1C` (ISO-BMFF): 4 bytes de
/// configuración (`0x81` = marcador + versión 1) seguidos de las OBU.
fn sequence_parameter_sets(codec: VideoCodec, sh: &[u8]) -> Vec<u8> {
    match codec {
        VideoCodec::Av1 if sh.first() == Some(&0x81) && sh.len() > 4 => {
            bitstream::parameter_sets(codec, &sh[4..])
        }
        _ => bitstream::parameter_sets(codec, sh),
    }
}

/// Formatea un VARIANT como "valor (vt=N)".
fn fmt_variant(v: &VARIANT) -> String {
    // SAFETY: lectura del discriminante `vt` de un VARIANT inicializado.
    let vt = unsafe { v.as_raw().Anonymous.Anonymous.vt };
    format!("{v} (vt={vt})")
}

/// Intenta fijar una propiedad `ICodecAPI` y devuelve el informe.
pub fn set_codec_property(
    api: &ICodecAPI,
    name: &'static str,
    guid: &GUID,
    value: &VARIANT,
    stage: &'static str,
) -> PropertyReport {
    let requested = fmt_variant(value);
    // SAFETY: llamadas COM con punteros a GUID/VARIANT locales válidos.
    let outcome = unsafe {
        if api.IsSupported(guid).is_err() {
            PropertyOutcome::NotSupported
        } else {
            match api.SetValue(guid, value) {
                Ok(()) => PropertyOutcome::Accepted {
                    readback: api.GetValue(guid).ok().map(|v| fmt_variant(&v)),
                },
                Err(e) => PropertyOutcome::Rejected {
                    hresult: e.code().0,
                },
            }
        }
    };
    PropertyReport {
        name,
        requested,
        stage,
        outcome,
    }
}

/// Consulta una propiedad sin fijarla (valor actual, rango y si es modificable).
pub fn query_codec_property(api: &ICodecAPI, name: &'static str, guid: &GUID) -> PropertyReport {
    // SAFETY: llamadas COM con punteros a GUID/VARIANT locales válidos.
    let outcome = unsafe {
        if api.IsSupported(guid).is_err() {
            PropertyOutcome::NotSupported
        } else {
            let mut parts = Vec::new();
            match api.GetValue(guid) {
                Ok(v) => parts.push(format!("valor={}", fmt_variant(&v))),
                Err(e) => parts.push(format!("GetValue=0x{:08X}", e.code().0 as u32)),
            }
            if let Ok(v) = api.GetDefaultValue(guid) {
                parts.push(format!("defecto={}", fmt_variant(&v)));
            }
            let (mut lo, mut hi, mut step) = (VARIANT::new(), VARIANT::new(), VARIANT::new());
            if api
                .GetParameterRange(guid, &mut lo, &mut hi, &mut step)
                .is_ok()
            {
                parts.push(format!("rango=[{lo}..{hi} paso {step}]"));
            }
            let m = api.IsModifiable(guid);
            parts.push(format!("modificable={}", m.is_ok() && m.0 == 0));
            PropertyOutcome::Queried {
                value: Some(parts.join("; ")),
            }
        }
    };
    PropertyReport {
        name,
        requested: String::new(),
        stage: "consulta",
        outcome,
    }
}

/// Eventos del MFT reenviados desde el hilo de eventos.
#[derive(Debug)]
enum MftEvent {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Other(u32),
    Failed { event: u32, hresult: i32 },
    GeneratorError(i32),
}

/// Envoltorio para mover el generador de eventos a su hilo.
struct SendGen(IMFMediaEventGenerator);
// SAFETY: los MFT asíncronos deben ser *free-threaded* (requisito documentado
// del modelo asíncrono de MF); `GetEvent` se llama solo desde el hilo de eventos.
unsafe impl Send for SendGen {}

fn spawn_event_thread(gen: IMFMediaEventGenerator) -> (Receiver<MftEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let gen = SendGen(gen);
    let handle = std::thread::Builder::new()
        .name("cs-encode-mft-events".into())
        .spawn(move || {
            let gen = gen;
            // SAFETY: COM MTA para este hilo.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            loop {
                // SAFETY: bloquea hasta que haya evento o el MFT se apague.
                let ev = match unsafe { gen.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                    Ok(ev) => ev,
                    Err(e) => {
                        let _ = tx.send(MftEvent::GeneratorError(e.code().0));
                        break;
                    }
                };
                // SAFETY: evento válido.
                let t = unsafe { ev.GetType() }.unwrap_or(0);
                // SAFETY: ídem.
                let status = unsafe { ev.GetStatus() }.map(|h| h.0).unwrap_or(0);
                let msg = if status < 0 {
                    MftEvent::Failed {
                        event: t,
                        hresult: status,
                    }
                } else if t == METransformNeedInput.0 as u32 {
                    MftEvent::NeedInput
                } else if t == METransformHaveOutput.0 as u32 {
                    MftEvent::HaveOutput
                } else if t == METransformDrainComplete.0 as u32 {
                    MftEvent::DrainComplete
                } else {
                    MftEvent::Other(t)
                };
                if tx.send(msg).is_err() {
                    break;
                }
            }
        })
        .expect("crear hilo de eventos del MFT");
    (rx, handle)
}

fn frame_size(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

fn set_video_common(t: &IMFMediaType, cfg: &EncoderConfig) -> windows::core::Result<()> {
    // SAFETY: atributos sobre un tipo de medio válido.
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, frame_size(cfg.fps_num, cfg.fps_den))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, frame_size(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
    }
    Ok(())
}

fn profile_value(p: H264Profile) -> u32 {
    match p {
        H264Profile::Baseline => eAVEncH264VProfile_Base.0 as u32,
        H264Profile::Main => eAVEncH264VProfile_Main.0 as u32,
        H264Profile::High => eAVEncH264VProfile_High.0 as u32,
    }
}

/// Estadísticas acumuladas del codificador.
#[derive(Debug, Clone, Default)]
pub struct EncoderStats {
    pub frames_in: u64,
    pub frames_out: u64,
    /// Salidas que llegaron después de agotarse `output_timeout`.
    pub late_outputs: u64,
    /// IDR a los que hubo que anteponer SPS/PPS.
    pub parameter_sets_prepended: u64,
    /// Renegociaciones `MF_E_TRANSFORM_STREAM_CHANGE`.
    pub stream_changes: u64,
}

/// Codificador H.264 por hardware (MFT de Media Foundation).
pub struct MfH264Encoder {
    cfg: EncoderConfig,
    converter: Nv12Converter,
    transform: IMFTransform,
    activate: IMFActivate,
    codec_api: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    events: Receiver<MftEvent>,
    event_thread: Option<JoinHandle<()>>,
    in_stream: u32,
    out_stream: u32,
    out_info: MFT_OUTPUT_STREAM_INFO,
    need_input: u32,
    pending: VecDeque<(u64, Instant, Instant)>,
    parameter_sets: Vec<u8>,
    current_bitrate_kbps: u32,
    caps: EncoderCaps,
    stats: EncoderStats,
    runtime_reported_bitrate: bool,
    runtime_reported_idr: bool,
    sequence_header: Vec<u8>,
    timer_raised: bool,
}

impl std::fmt::Debug for MfH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfH264Encoder")
            .field("cfg", &self.cfg)
            .field("encoder", &self.caps.info.friendly_name)
            .field("stats", &self.stats)
            .finish()
    }
}

impl MfH264Encoder {
    /// Crea el codificador sobre `device` (del adaptador que se capturará).
    pub fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self> {
        Self::new_with_properties(device, cfg, &[])
    }

    /// Como [`MfH264Encoder::new`], pero fija además `extra` por `ICodecAPI`
    /// **antes** de los tipos de medio (tras empezar el streaming el MFT de
    /// Intel acepta `SetValue` pero ignora la mayoría de propiedades).
    pub fn new_with_properties(
        device: &ID3D11Device,
        cfg: EncoderConfig,
        extra: &[(&'static str, GUID, VARIANT)],
    ) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps_num == 0 || cfg.fps_den == 0 {
            return Err(EncodeError::BadInput("dimensiones o fps nulos".into()));
        }
        mf_startup()?;
        let raise = cfg.raise_timer_resolution;
        match Self::build(device, cfg, extra) {
            Ok(mut enc) => {
                if raise {
                    honor_timer_resolution();
                    // SAFETY: sin punteros; se empareja con timeEndPeriod en Drop.
                    enc.timer_raised = unsafe { timeBeginPeriod(1) } == 0;
                }
                Ok(enc)
            }
            Err(e) => {
                // SAFETY: empareja el MFStartup anterior.
                let _ = unsafe { MFShutdown() };
                Err(e)
            }
        }
    }

    fn build(
        device: &ID3D11Device,
        cfg: EncoderConfig,
        extra: &[(&'static str, GUID, VARIANT)],
    ) -> Result<Self> {
        enable_multithread_protection(device)?;

        // Gestor DXGI compartido con el MFT.
        let mut token = 0u32;
        let mut manager = None;
        // SAFETY: punteros de salida locales válidos.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| hr_err("MFCreateDXGIDeviceManager", e))?;
        let manager = manager.ok_or(EncodeError::NoEncoder("gestor DXGI nulo".into()))?;
        // SAFETY: dispositivo válido; token devuelto por MF.
        unsafe { manager.ResetDevice(device, token) }
            .map_err(|e| hr_err("IMFDXGIDeviceManager::ResetDevice", e))?;

        // Elegir MFT: mismo LUID que el dispositivo; si no, el primero.
        let luid = device_luid(device);
        let mut list = enumerate_hw_encoders(cfg.codec)?;
        if list.is_empty() {
            return Err(EncodeError::NoEncoder(format!(
                "MFTEnumEx no devolvió ningún MFT {:?} hardware",
                cfg.codec
            )));
        }
        for (info, _) in &list {
            tracing::info!(
                name = %info.friendly_name,
                vendor = ?info.vendor_id,
                luid = ?info.adapter_luid,
                "MFT H.264 hardware encontrado"
            );
        }
        let idx = luid
            .and_then(|l| list.iter().position(|(i, _)| i.adapter_luid == Some(l)))
            .unwrap_or(0);
        let (info, activate) = list.swap_remove(idx);

        // SAFETY: activación del objeto COM del MFT.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(|e| hr_err("ActivateObject", e))?;
        // SAFETY: atributos del MFT.
        let attrs = unsafe { transform.GetAttributes() }.map_err(|e| hr_err("GetAttributes", e))?;
        // SAFETY: lecturas/escrituras de atributos UINT32.
        let async_mft = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if !async_mft {
            return Err(EncodeError::Unsupported(format!(
                "el MFT '{}' no es asíncrono; esta implementación solo maneja el modelo asíncrono",
                info.friendly_name
            )));
        }
        // SAFETY: ídem.
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(|e| hr_err("SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK)", e))?;
        // SAFETY: ídem.
        let d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        if !d3d11_aware {
            return Err(EncodeError::Unsupported(format!(
                "el MFT '{}' no declara MF_SA_D3D11_AWARE",
                info.friendly_name
            )));
        }
        // SAFETY: el MFT toma su propia referencia al gestor (IUnknown en ulParam).
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .map_err(|e| hr_err("ProcessMessage(SET_D3D_MANAGER)", e))?;

        // Identificadores de flujo (E_NOTIMPL ⇒ 0 y 0).
        let (mut in_ids, mut out_ids) = ([0u32], [0u32]);
        // SAFETY: arrays locales de longitud 1 (un flujo de entrada y uno de salida).
        let (in_stream, out_stream) =
            match unsafe { transform.GetStreamIDs(&mut in_ids, &mut out_ids) } {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0),
            };

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        let mut properties = Vec::new();

        // Propiedades que se piden (antes de los tipos; si se rechazan, se
        // reintenta después de fijarlos).
        let bps = cfg.bitrate_kbps.saturating_mul(1000);
        let mut wanted: Vec<(&'static str, GUID, VARIANT)> = Vec::new();
        if cfg.display_remoting_scenario {
            wanted.push((
                "CODECAPI_AVScenarioInfo=DisplayRemoting",
                CODECAPI_AVScenarioInfo,
                VARIANT::from(eAVScenarioInfo_DisplayRemoting.0 as u32),
            ));
        }
        wanted.extend([
            (
                "CODECAPI_AVLowLatencyMode",
                CODECAPI_AVLowLatencyMode,
                VARIANT::from(cfg.low_latency),
            ),
            (
                "CODECAPI_AVEncCommonRateControlMode=CBR",
                CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            ),
            (
                "CODECAPI_AVEncCommonMeanBitRate",
                CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(bps),
            ),
            (
                "CODECAPI_AVEncMPVGOPSize",
                CODECAPI_AVEncMPVGOPSize,
                VARIANT::from(cfg.gop_frames),
            ),
            (
                "CODECAPI_AVEncMPVDefaultBPictureCount=0",
                CODECAPI_AVEncMPVDefaultBPictureCount,
                VARIANT::from(0u32),
            ),
        ]);
        // Perfil: H.264 el pedido; HEVC Main 4:2:0 8 bits; AV1 el del driver.
        match cfg.codec {
            VideoCodec::H264 => wanted.push((
                "CODECAPI_AVEncMPVProfile",
                CODECAPI_AVEncMPVProfile,
                VARIANT::from(profile_value(cfg.profile)),
            )),
            VideoCodec::Hevc => wanted.push((
                "CODECAPI_AVEncMPVProfile (HEVC Main)",
                CODECAPI_AVEncMPVProfile,
                VARIANT::from(eAVEncH265VProfile_Main_420_8.0 as u32),
            )),
            VideoCodec::Av1 => {}
        }
        if let Some(frames) = cfg.vbv_frames {
            let bits = (bps as f64 * frames as f64 * cfg.fps_den as f64 / cfg.fps_num as f64)
                .round() as u32;
            wanted.push((
                "CODECAPI_AVEncCommonBufferSize (VBV)",
                CODECAPI_AVEncCommonBufferSize,
                VARIANT::from(bits),
            ));
        }
        if cfg.codec == VideoCodec::H264 && cfg.profile != H264Profile::Baseline {
            wanted.push((
                "CODECAPI_AVEncH264CABACEnable",
                CODECAPI_AVEncH264CABACEnable,
                VARIANT::from(true),
            ));
        }
        if let Some(ir) = cfg.intra_refresh {
            wanted.push((
                "CODECAPI_AVEncVideoGradualIntraRefresh",
                CODECAPI_AVEncVideoGradualIntraRefresh,
                VARIANT::from(ir),
            ));
        }

        wanted.extend(extra.iter().cloned());
        let mut retry = Vec::new();
        if let Some(api) = &codec_api {
            for (name, guid, value) in &wanted {
                let r = set_codec_property(api, name, guid, value, "antes de los tipos");
                if matches!(r.outcome, PropertyOutcome::Rejected { .. }) {
                    retry.push((r, *name, *guid, value.clone()));
                } else {
                    properties.push(r);
                }
            }
        }

        // Tipo de salida (el códec) primero, como exige el codificador.
        // SAFETY: creación y configuración de tipos de medio.
        let out_type =
            unsafe { MFCreateMediaType() }.map_err(|e| hr_err("MFCreateMediaType", e))?;
        set_video_common(&out_type, &cfg).map_err(|e| hr_err("tipo de salida", e))?;
        // SAFETY: atributos sobre un tipo válido.
        unsafe {
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &codec_subtype(cfg.codec))
                .and_then(|_| out_type.SetUINT32(&MF_MT_AVG_BITRATE, bps))
                .and_then(|_| match cfg.codec {
                    VideoCodec::H264 => {
                        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile_value(cfg.profile))
                    }
                    VideoCodec::Hevc => out_type
                        .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32),
                    VideoCodec::Av1 => Ok(()),
                })
        }
        .map_err(|e| hr_err("tipo de salida", e))?;
        // SAFETY: tipo válido.
        unsafe { transform.SetOutputType(out_stream, &out_type, 0) }
            .map_err(|e| hr_err("SetOutputType(códec)", e))?;

        // Tipo de entrada: el NV12 que ofrezca el MFT, con nuestras dimensiones.
        let mut in_type = None;
        for i in 0.. {
            // SAFETY: enumeración por índice; termina con MF_E_NO_MORE_TYPES.
            let Ok(t) = (unsafe { transform.GetInputAvailableType(in_stream, i) }) else {
                break;
            };
            // SAFETY: lectura de atributo GUID.
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                in_type = Some(t);
                break;
            }
        }
        let in_type = match in_type {
            Some(t) => t,
            // SAFETY: creación de tipo nuevo.
            None => unsafe { MFCreateMediaType() }.map_err(|e| hr_err("MFCreateMediaType", e))?,
        };
        set_video_common(&in_type, &cfg).map_err(|e| hr_err("tipo de entrada", e))?;
        // SAFETY: atributo GUID sobre tipo válido.
        unsafe { in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }
            .map_err(|e| hr_err("tipo de entrada NV12", e))?;
        // SAFETY: tipo válido.
        unsafe { transform.SetInputType(in_stream, &in_type, 0) }
            .map_err(|e| hr_err("SetInputType(NV12)", e))?;

        if let Some(api) = &codec_api {
            for (first, name, guid, value) in retry {
                let mut r = set_codec_property(api, name, &guid, &value, "después de los tipos");
                if let (
                    PropertyOutcome::Rejected { hresult: a },
                    PropertyOutcome::Rejected { .. },
                ) = (&first.outcome, &r.outcome)
                {
                    r.requested = format!("{} (antes: 0x{:08X})", r.requested, *a as u32);
                }
                properties.push(r);
            }
            for (name, guid) in [
                (
                    "CODECAPI_AVEncCommonQualityVsSpeed",
                    CODECAPI_AVEncCommonQualityVsSpeed,
                ),
                (
                    "CODECAPI_AVEncCommonMaxBitRate",
                    CODECAPI_AVEncCommonMaxBitRate,
                ),
                (
                    "CODECAPI_AVEncVideoForceKeyFrame",
                    CODECAPI_AVEncVideoForceKeyFrame,
                ),
                (
                    "CODECAPI_AVEncVideoTemporalLayerCount",
                    CODECAPI_AVEncVideoTemporalLayerCount,
                ),
                (
                    "CODECAPI_AVEncVideoGradualIntraRefresh",
                    CODECAPI_AVEncVideoGradualIntraRefresh,
                ),
                (
                    "CODECAPI_AVEncVideoLTRBufferControl",
                    CODECAPI_AVEncVideoLTRBufferControl,
                ),
                (
                    "CODECAPI_AVEncSliceControlMode",
                    CODECAPI_AVEncSliceControlMode,
                ),
                (
                    "CODECAPI_AVEncSliceControlSize",
                    CODECAPI_AVEncSliceControlSize,
                ),
                (
                    "CODECAPI_AVEncVideoMaxNumRefFrame",
                    CODECAPI_AVEncVideoMaxNumRefFrame,
                ),
                ("CODECAPI_AVEncVideoMinQP", CODECAPI_AVEncVideoMinQP),
                ("CODECAPI_AVEncVideoMaxQP", CODECAPI_AVEncVideoMaxQP),
                (
                    "CODECAPI_AVEncVideoEncodeFrameTypeQP",
                    CODECAPI_AVEncVideoEncodeFrameTypeQP,
                ),
                (
                    "CODECAPI_AVEncNumWorkerThreads",
                    CODECAPI_AVEncNumWorkerThreads,
                ),
                ("CODECAPI_AVEncAdaptiveMode", CODECAPI_AVEncAdaptiveMode),
                ("CODECAPI_AVEncCommonRealTime", CODECAPI_AVEncCommonRealTime),
                ("CODECAPI_AVEncH264SPSID", CODECAPI_AVEncH264SPSID),
                (
                    "CODECAPI_AVEncCommonLowLatency",
                    CODECAPI_AVEncCommonLowLatency,
                ),
            ] {
                properties.push(query_codec_property(api, name, &guid));
            }
        }

        // SAFETY: tipo de salida actual (puede traer SPS/PPS).
        let cur_out = unsafe { transform.GetOutputCurrentType(out_stream) }
            .map_err(|e| hr_err("GetOutputCurrentType", e))?;
        let sequence_header = read_sequence_header(&cur_out);

        // SAFETY: información del flujo de salida.
        let out_info = unsafe { transform.GetOutputStreamInfo(out_stream) }
            .map_err(|e| hr_err("GetOutputStreamInfo", e))?;
        let provides = out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let converter = Nv12Converter::new(
            device,
            (cfg.width, cfg.height),
            (cfg.width, cfg.height),
            (cfg.fps_num, cfg.fps_den),
            8,
        )?;

        let gen: IMFMediaEventGenerator = transform
            .cast()
            .map_err(|e| hr_err("IMFTransform→IMFMediaEventGenerator", e))?;
        let (events, handle) = spawn_event_thread(gen);

        // SAFETY: mensajes de inicio de streaming.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| hr_err("NOTIFY_BEGIN_STREAMING", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| hr_err("NOTIFY_START_OF_STREAM", e))?;
        }

        let caps = EncoderCaps {
            info,
            async_mft,
            d3d11_aware,
            provides_output_samples: provides,
            sequence_header_in_type: !sequence_header.is_empty(),
            properties,
        };
        let parameter_sets = sequence_parameter_sets(cfg.codec, &sequence_header);
        Ok(Self {
            current_bitrate_kbps: cfg.bitrate_kbps,
            cfg,
            converter,
            transform,
            activate,
            codec_api,
            _manager: manager,
            events,
            event_thread: Some(handle),
            in_stream,
            out_stream,
            out_info,
            need_input: 0,
            pending: VecDeque::new(),
            parameter_sets,
            caps,
            stats: EncoderStats::default(),
            runtime_reported_bitrate: false,
            runtime_reported_idr: false,
            sequence_header,
            timer_raised: false,
        })
    }

    /// Estadísticas acumuladas.
    pub fn stats(&self) -> &EncoderStats {
        &self.stats
    }

    /// `MF_MT_MPEG_SEQUENCE_HEADER` del tipo de salida (Annex-B), si lo hubo.
    pub fn sequence_header(&self) -> &[u8] {
        &self.sequence_header
    }

    /// Interfaz `ICodecAPI` del MFT, para experimentos con propiedades.
    pub fn codec_api(&self) -> Option<&ICodecAPI> {
        self.codec_api.as_ref()
    }

    /// Conversor NV12 (para medirlo aparte).
    pub fn converter_mut(&mut self) -> &mut Nv12Converter {
        &mut self.converter
    }

    /// Banderas de enlace de las texturas NV12.
    pub fn nv12_bind_flags(&self) -> u32 {
        self.converter.nv12_bind_flags
    }

    fn handle_event(&mut self, ev: MftEvent, out: &mut Vec<EncodedUnit>) -> Result<()> {
        match ev {
            MftEvent::NeedInput => self.need_input += 1,
            MftEvent::HaveOutput => {
                if let Some(u) = self.process_output()? {
                    out.push(u);
                }
            }
            MftEvent::DrainComplete => {}
            MftEvent::Other(t) => tracing::debug!(event = t, "evento MFT ignorado"),
            MftEvent::Failed { event, hresult } => {
                tracing::error!(
                    event,
                    hresult = format!("0x{:08X}", hresult as u32),
                    "evento MFT con error"
                );
                return Err(EncodeError::Os {
                    call: "evento del MFT con estado de error",
                    hresult: hresult as u32,
                });
            }
            MftEvent::GeneratorError(h) => {
                return Err(EncodeError::Os {
                    call: "IMFMediaEventGenerator::GetEvent",
                    hresult: h as u32,
                })
            }
        }
        Ok(())
    }

    /// Espera un evento hasta `deadline`. `false` si se agotó el tiempo.
    fn wait_event(&mut self, deadline: Instant, out: &mut Vec<EncodedUnit>) -> Result<bool> {
        let now = Instant::now();
        let timeout = deadline.saturating_duration_since(now);
        match self.events.recv_timeout(timeout) {
            Ok(ev) => {
                self.handle_event(ev, out)?;
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => Err(EncodeError::Os {
                call: "hilo de eventos del MFT terminado",
                hresult: MF_E_SHUTDOWN.0 as u32,
            }),
        }
    }

    fn drain_ready_events(&mut self, out: &mut Vec<EncodedUnit>) -> Result<()> {
        while let Ok(ev) = self.events.try_recv() {
            self.handle_event(ev, out)?;
        }
        Ok(())
    }

    fn renegotiate_output(&mut self) -> Result<()> {
        self.stats.stream_changes += 1;
        // SAFETY: el MFT ofrece el nuevo tipo en el índice 0.
        let t = unsafe { self.transform.GetOutputAvailableType(self.out_stream, 0) }
            .map_err(|e| hr_err("GetOutputAvailableType", e))?;
        // SAFETY: tipo válido.
        unsafe { self.transform.SetOutputType(self.out_stream, &t, 0) }
            .map_err(|e| hr_err("SetOutputType (renegociación)", e))?;
        let sh = read_sequence_header(&t);
        if !sh.is_empty() {
            self.parameter_sets = sequence_parameter_sets(self.cfg.codec, &sh);
            self.sequence_header = sh;
            self.caps.sequence_header_in_type = true;
        }
        // SAFETY: información del flujo.
        self.out_info = unsafe { self.transform.GetOutputStreamInfo(self.out_stream) }
            .map_err(|e| hr_err("GetOutputStreamInfo", e))?;
        Ok(())
    }

    fn process_output(&mut self) -> Result<Option<EncodedUnit>> {
        let provides = self.out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let mut own_sample = None;
        if !provides {
            let size = self
                .out_info
                .cbSize
                .max(self.cfg.width * self.cfg.height * 3 / 2);
            // SAFETY: creación de muestra y búfer en memoria.
            unsafe {
                let s = MFCreateSample().map_err(|e| hr_err("MFCreateSample", e))?;
                let b =
                    MFCreateMemoryBuffer(size).map_err(|e| hr_err("MFCreateMemoryBuffer", e))?;
                s.AddBuffer(&b).map_err(|e| hr_err("AddBuffer", e))?;
                own_sample = Some(s);
            }
        }
        let mut buf = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.out_stream,
            pSample: ManuallyDrop::new(own_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: array de 1 búfer válido durante la llamada.
        let r = unsafe { self.transform.ProcessOutput(0, &mut buf, &mut status) };
        // SAFETY: recuperamos la propiedad de las referencias COM del búfer
        // (la muestra, propia o del MFT, y la colección de eventos).
        let sample = unsafe { ManuallyDrop::take(&mut buf[0].pSample) };
        // SAFETY: ídem.
        drop(unsafe { ManuallyDrop::take(&mut buf[0].pEvents) });
        if let Err(e) = r {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                self.renegotiate_output()?;
                return Ok(None);
            }
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(hr_err("ProcessOutput", e));
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        // SAFETY: lecturas sobre una muestra válida.
        let (time_hns, clean_point, data) = unsafe {
            let t = sample.GetSampleTime().unwrap_or(0);
            let cp = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let b = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| hr_err("ConvertToContiguousBuffer", e))?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            b.Lock(&mut p, None, Some(&mut len))
                .map_err(|e| hr_err("IMFMediaBuffer::Lock", e))?;
            let v = std::slice::from_raw_parts(p, len as usize).to_vec();
            let _ = b.Unlock();
            (t, cp, v)
        };
        let now = Instant::now();
        let pts_us = (time_hns.max(0) as u64) / 10;
        let times = match self.pending.iter().position(|(p, _, _)| *p == pts_us) {
            Some(i) => self.pending.remove(i).map(|(_, a, b)| (a, b)),
            None => None,
        };
        self.stats.frames_out += 1;

        let codec = self.cfg.codec;
        let info = bitstream::inspect(codec, &data);
        let keyframe = clean_point || info.keyframe;
        let mut data = data;
        if info.has_parameter_sets {
            self.parameter_sets = bitstream::parameter_sets(codec, &data);
        } else if keyframe
            && self.cfg.ensure_parameter_sets_on_idr
            && !self.parameter_sets.is_empty()
        {
            data = bitstream::prepend_parameter_sets(codec, &self.parameter_sets, &data);
            self.stats.parameter_sets_prepended += 1;
        }
        Ok(Some(EncodedUnit {
            data,
            keyframe,
            pts_us,
            encode_time: times.map_or(Duration::ZERO, |(s, _)| now - s),
            hw_time: times.map_or(Duration::ZERO, |(_, s)| now - s),
        }))
    }

    fn apply_control(&mut self, ctl: EncodeControl) {
        let Some(api) = self.codec_api.clone() else {
            return;
        };
        if let Some(kbps) = ctl.bitrate_kbps.filter(|k| *k != self.current_bitrate_kbps) {
            let r = set_codec_property(
                &api,
                "CODECAPI_AVEncCommonMeanBitRate (dinámico)",
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(kbps.saturating_mul(1000)),
                "en streaming",
            );
            if matches!(r.outcome, PropertyOutcome::Accepted { .. }) {
                self.current_bitrate_kbps = kbps;
            } else {
                tracing::warn!(?r, "cambio de tasa rechazado");
            }
            if !self.runtime_reported_bitrate {
                self.runtime_reported_bitrate = true;
                self.caps.properties.push(r);
            }
        }
        if ctl.force_idr {
            let r = set_codec_property(
                &api,
                "CODECAPI_AVEncVideoForceKeyFrame",
                &CODECAPI_AVEncVideoForceKeyFrame,
                &VARIANT::from(1u32),
                "en streaming",
            );
            if !matches!(r.outcome, PropertyOutcome::Accepted { .. }) {
                tracing::warn!(?r, "IDR forzado rechazado");
            }
            if !self.runtime_reported_idr {
                self.runtime_reported_idr = true;
                self.caps.properties.push(r);
            }
        }
    }

    /// Tasa objetivo actual (kbit/s).
    pub fn bitrate_kbps(&self) -> u32 {
        self.current_bitrate_kbps
    }
}

/// Pide a Windows que respete `timeBeginPeriod` y no aplique EcoQoS a este
/// proceso aunque no tenga ventana visible (Windows 11 ignora la resolución
/// del temporizador de procesos ocultos u ocluidos). Fallo no fatal.
fn honor_timer_resolution() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION
            | PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: puntero a una estructura local del tamaño indicado.
    let r = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "SetProcessInformation(ProcessPowerThrottling) falló");
    }
}

fn read_sequence_header(t: &IMFMediaType) -> Vec<u8> {
    // SAFETY: lectura de blob en un búfer del tamaño anunciado.
    unsafe {
        let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
            return Vec::new();
        };
        let mut v = vec![0u8; n as usize];
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None)
            .is_err()
        {
            return Vec::new();
        }
        v
    }
}

impl VideoEncoder for MfH264Encoder {
    type Frame = InputFrame;

    fn encode(&mut self, frame: &InputFrame, ctl: EncodeControl) -> Result<Vec<EncodedUnit>> {
        let t0 = Instant::now();
        let mut out = Vec::new();
        self.drain_ready_events(&mut out)?;

        // BGRA → NV12 en la GPU.
        let nv12 = self.converter.convert(&frame.texture, frame.subresource)?;
        self.converter.flush();

        // Esperar a que el MFT pida entrada (modelo asíncrono).
        let deadline = t0 + self.cfg.output_timeout;
        while self.need_input == 0 {
            if !self.wait_event(deadline, &mut out)? {
                return Err(EncodeError::Timeout("METransformNeedInput"));
            }
        }

        self.apply_control(ctl);

        // SAFETY: muestra con un búfer DXGI que referencia la textura NV12.
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, FALSE)
                .map_err(|e| hr_err("MFCreateDXGISurfaceBuffer", e))?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(len);
                }
            }
            let s = MFCreateSample().map_err(|e| hr_err("MFCreateSample", e))?;
            s.AddBuffer(&buffer).map_err(|e| hr_err("AddBuffer", e))?;
            s.SetSampleTime(frame.pts_us as i64 * 10)
                .map_err(|e| hr_err("SetSampleTime", e))?;
            s.SetSampleDuration(self.cfg.frame_duration_us() as i64 * 10)
                .map_err(|e| hr_err("SetSampleDuration", e))?;
            s
        };
        let t_submit = Instant::now();
        // SAFETY: muestra válida; el MFT pidió entrada.
        unsafe { self.transform.ProcessInput(self.in_stream, &sample, 0) }
            .map_err(|e| hr_err("ProcessInput", e))?;
        self.need_input -= 1;
        self.stats.frames_in += 1;
        self.pending.push_back((frame.pts_us, t0, t_submit));
        // Evitar crecimiento sin límite si el MFT descarta frames.
        while self.pending.len() > 64 {
            self.pending.pop_front();
        }

        // Esperar la salida de este frame (baja latencia: 1 entrada → 1 salida).
        let got = |out: &Vec<EncodedUnit>| out.iter().any(|u| u.pts_us == frame.pts_us);
        while !got(&out) {
            if !self.wait_event(deadline, &mut out)? {
                break;
            }
        }
        self.drain_ready_events(&mut out)?;
        for u in &out {
            if u.pts_us != frame.pts_us {
                self.stats.late_outputs += 1;
            }
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<EncodedUnit>> {
        let mut out = Vec::new();
        // SAFETY: mensaje de vaciado al MFT.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| hr_err("COMMAND_DRAIN", e))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(MftEvent::DrainComplete) => break,
                Ok(ev) => self.handle_event(ev, &mut out)?,
                Err(_) => return Err(EncodeError::Timeout("METransformDrainComplete")),
            }
        }
        Ok(out)
    }

    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        // SAFETY: secuencia de cierre del MFT; los errores se ignoran.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.in_stream as usize);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        // IMFShutdown desbloquea el GetEvent del hilo de eventos (MF_E_SHUTDOWN).
        let shut = match self.transform.cast::<IMFShutdown>() {
            // SAFETY: interfaz válida.
            Ok(s) => unsafe { s.Shutdown() }.is_ok(),
            Err(_) => false,
        };
        if let Some(h) = self.event_thread.take() {
            if shut {
                let _ = h.join();
            } else {
                tracing::warn!("el MFT no implementa IMFShutdown; el hilo de eventos queda suelto");
            }
        }
        if self.timer_raised {
            // SAFETY: empareja el timeBeginPeriod(1) de `new`.
            unsafe { timeEndPeriod(1) };
        }
        // SAFETY: libera el objeto activado y empareja MFStartup.
        unsafe {
            let _ = self.activate.ShutdownObject();
            let _ = MFShutdown();
        }
    }
}
