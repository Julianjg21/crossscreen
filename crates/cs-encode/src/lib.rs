//! Codificación de vídeo H.264 de baja latencia para CrossScreen.
//!
//! - [`annexb`] y [`h264`]: análisis del flujo de bits independiente del SO
//!   (separación de NAL Annex-B, SPS con VUI, cabecera de *slice*).
//! - `mf` (solo Windows): codificador por hardware con Media Foundation
//!   (MFT H.264 asíncrono del fabricante), entrada `ID3D11Texture2D` BGRA y
//!   conversión BGRA→NV12 en la GPU con el Video Processor de D3D11.
//!
//! El contrato común es el *trait* [`VideoEncoder`]: un frame entra y salen
//! cero o más [`EncodedUnit`] en Annex-B. Con el modo de baja latencia la
//! salida de un frame aparece en la misma llamada a `encode`.

pub mod annexb;
pub mod h264;

#[cfg(windows)]
pub mod mf;

use std::time::Duration;

/// Perfil H.264 pedido al codificador.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// Constrained Baseline (sin CABAC ni frames B).
    Baseline,
    /// Main.
    Main,
    /// High (8x8 transform; el habitual en escritorio).
    High,
}

impl H264Profile {
    /// Valor de `profile_idc` en el SPS.
    pub fn profile_idc(self) -> u8 {
        match self {
            H264Profile::Baseline => 66,
            H264Profile::Main => 77,
            H264Profile::High => 100,
        }
    }
}

/// Configuración inicial del codificador.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Tasa media objetivo (CBR) en kbit/s.
    pub bitrate_kbps: u32,
    /// Distancia entre IDR periódicos en frames. Para escritorio remoto se usa
    /// un valor grande y se fuerzan IDR bajo demanda (pérdidas, receptor nuevo).
    pub gop_frames: u32,
    pub profile: H264Profile,
    /// Tamaño del búfer VBV (`CODECAPI_AVEncCommonBufferSize`) en frames de
    /// tasa media. `None` deja el valor del driver.
    pub vbv_frames: Option<f32>,
    /// Pide `CODECAPI_AVScenarioInfo = DisplayRemoting` (escritorio remoto).
    pub display_remoting_scenario: bool,
    /// Sube la resolución del temporizador del proceso a 1 ms
    /// (`timeBeginPeriod(1)`) mientras viva el codificador. Hallazgo de la
    /// PoC-3: con la resolución por defecto (15,6 ms) el MFT de Intel tarda
    /// ~16 ms por paso del modelo asíncrono y la latencia sube a ~30 ms.
    pub raise_timer_resolution: bool,
    /// Activa `CODECAPI_AVLowLatencyMode` (1 entrada → 1 salida, sin *lookahead*).
    pub low_latency: bool,
    /// Cuánto espera `encode` a que salga el frame recién enviado antes de
    /// devolver lo que haya. La salida tardía se devuelve en la siguiente llamada.
    pub output_timeout: Duration,
    /// Si un IDR sale sin SPS/PPS en banda, anteponer los últimos conocidos
    /// (de `MF_MT_MPEG_SEQUENCE_HEADER` o de un IDR anterior).
    pub ensure_parameter_sets_on_idr: bool,
    /// Valor opcional para `CODECAPI_AVEncVideoGradualIntraRefresh` (`VT_UI4`).
    /// `None` no toca la propiedad (queda en 0, desactivado).
    pub intra_refresh: Option<u32>,
}

impl EncoderConfig {
    /// Configuración razonable para escritorio remoto: CBR, High, GOP de 10 s.
    pub fn desktop(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            bitrate_kbps,
            gop_frames: fps.saturating_mul(10).max(1),
            profile: H264Profile::High,
            vbv_frames: Some(1.0),
            display_remoting_scenario: true,
            raise_timer_resolution: true,
            low_latency: true,
            output_timeout: Duration::from_millis(100),
            ensure_parameter_sets_on_idr: true,
            intra_refresh: None,
        }
    }

    /// Duración nominal de un frame en microsegundos.
    pub fn frame_duration_us(&self) -> u64 {
        if self.fps_num == 0 {
            return 0;
        }
        1_000_000u64 * self.fps_den as u64 / self.fps_num as u64
    }
}

/// Órdenes por frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeControl {
    /// Pedir que este frame salga como IDR.
    pub force_idr: bool,
    /// Cambiar la tasa objetivo (kbit/s) a partir de este frame.
    pub bitrate_kbps: Option<u32>,
}

/// Un frame codificado (una unidad de acceso completa) en Annex-B.
#[derive(Debug, Clone)]
pub struct EncodedUnit {
    /// NAL con códigos de inicio `00 00 00 01` / `00 00 01`.
    pub data: Vec<u8>,
    /// IDR (según el atributo `CleanPoint` del MFT o, en su defecto, por las NAL).
    pub keyframe: bool,
    /// Marca de tiempo de presentación del frame de entrada (µs).
    pub pts_us: u64,
    /// Desde que `encode` recibió el frame (antes de la conversión NV12)
    /// hasta que la salida estuvo disponible en CPU.
    pub encode_time: Duration,
    /// Solo la parte del codificador: desde que se entregó la muestra NV12
    /// (`ProcessInput`) hasta que la salida estuvo disponible en CPU.
    pub hw_time: Duration,
}

/// Resultado de intentar fijar una propiedad `ICodecAPI`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyOutcome {
    /// `SetValue` devolvió S_OK. `readback` es el valor leído después, si se pudo.
    Accepted { readback: Option<String> },
    /// `IsSupported` dijo que no; no se intentó fijar.
    NotSupported,
    /// `IsSupported` dijo que sí pero `SetValue` falló con este HRESULT.
    Rejected { hresult: i32 },
    /// Solo se consultó (no se fija en esta configuración).
    Queried { value: Option<String> },
}

/// Informe de una propiedad del codificador (entregable clave de la PoC-3).
#[derive(Debug, Clone)]
pub struct PropertyReport {
    pub name: &'static str,
    /// Valor que se intentó fijar, en texto (vacío si solo se consultó).
    pub requested: String,
    /// Momento en que se fijó: antes de los tipos de medio, después, o en streaming.
    pub stage: &'static str,
    pub outcome: PropertyOutcome,
}

/// Descripción de un MFT codificador encontrado al enumerar.
#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub friendly_name: String,
    /// `VEN_8086` (Intel), `VEN_10DE` (NVIDIA), `VEN_1002` (AMD)…
    pub vendor_id: Option<String>,
    pub hardware: bool,
    /// LUID del adaptador al que pertenece el MFT, si lo anuncia.
    pub adapter_luid: Option<u64>,
}

/// Capacidades y comportamiento observado del codificador activo.
#[derive(Debug, Clone)]
pub struct EncoderCaps {
    pub info: EncoderInfo,
    /// El MFT es asíncrono (`MF_TRANSFORM_ASYNC`).
    pub async_mft: bool,
    /// El MFT acepta texturas D3D11 (`MF_SA_D3D11_AWARE`).
    pub d3d11_aware: bool,
    /// El MFT asigna las muestras de salida (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`).
    pub provides_output_samples: bool,
    /// El tipo de salida trae `MF_MT_MPEG_SEQUENCE_HEADER` (SPS/PPS).
    pub sequence_header_in_type: bool,
    pub properties: Vec<PropertyReport>,
}

/// Errores de codificación.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("{call} falló: HRESULT 0x{hresult:08X}")]
    Os { call: &'static str, hresult: u32 },
    #[error("no hay codificador H.264 por hardware disponible: {0}")]
    NoEncoder(String),
    #[error("no soportado: {0}")]
    Unsupported(String),
    #[error("tiempo de espera agotado: {0}")]
    Timeout(&'static str),
    #[error("el frame de entrada no es válido: {0}")]
    BadInput(String),
}

pub type Result<T> = std::result::Result<T, EncodeError>;

/// Contrato de un codificador de vídeo.
pub trait VideoEncoder {
    /// Tipo del frame de entrada (en Windows, una textura D3D11 BGRA).
    type Frame;

    /// Codifica un frame. Devuelve las unidades disponibles (normalmente la
    /// del propio frame; puede incluir salidas tardías de frames anteriores).
    fn encode(&mut self, frame: &Self::Frame, ctl: EncodeControl) -> Result<Vec<EncodedUnit>>;

    /// Vacía el codificador y devuelve las unidades pendientes.
    fn flush(&mut self) -> Result<Vec<EncodedUnit>>;

    /// Capacidades y propiedades aceptadas/rechazadas.
    fn caps(&self) -> &EncoderCaps;
}
