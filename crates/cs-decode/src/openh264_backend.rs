//! Decodificación H.264 por software con OpenH264 (Linux y otras plataformas
//! que no son Windows).
//!
//! - Se carga en tiempo de ejecución el **binario oficial de Cisco**
//!   (`libopenh264-2.6.0-linux64.8.so`): así la licencia de patentes de H.264
//!   la cubre Cisco. El crate `openh264` verifica su SHA-256 antes de usarlo.
//! - La salida son planos I420 en memoria de CPU (copiados: el búfer interno
//!   de OpenH264 solo es válido hasta la siguiente llamada). `cs-render` los
//!   sube a la GPU para presentarlos.
//!
//! Siguiente paso previsto: VAAPI por hardware con exportación DMA-BUF.

use crate::{DecodeError, Result};
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Nombre del binario de Cisco que espera esta versión del crate.
pub const OPENH264_BLOB: &str = "libopenh264-2.6.0-linux64.8.so";
/// Variable de entorno con la ruta al binario (tiene prioridad).
pub const OPENH264_ENV: &str = "CROSSSCREEN_OPENH264";

/// Contexto del decodificador: dónde está el binario de OpenH264.
#[derive(Debug, Clone)]
pub struct DecoderContext {
    blob: PathBuf,
}

impl DecoderContext {
    pub fn blob_path(&self) -> &Path {
        &self.blob
    }
}

/// Localiza el binario de OpenH264: `CROSSSCREEN_OPENH264`, junto al
/// ejecutable, o en `~/.local/share/crossscreen/`.
pub fn create_decoder_context() -> Result<DecoderContext> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os(OPENH264_ENV) {
        candidates.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(OPENH264_BLOB));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local/share/crossscreen")
                .join(OPENH264_BLOB),
        );
    }
    let blob = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            DecodeError::Unsupported(format!(
                "no se encontró {OPENH264_BLOB}. Descárgalo de Cisco con \
             scripts/fetch-openh264.sh o indica su ruta en {OPENH264_ENV}"
            ))
        })?;
    // Validar ya (hash incluido) para dar un error claro antes de la sesión.
    OpenH264API::from_blob_path(&blob).map_err(|e| {
        DecodeError::Unsupported(format!(
            "{} no es el binario esperado de OpenH264 ({e})",
            blob.display()
        ))
    })?;
    Ok(DecoderContext { blob })
}

/// Frame decodificado en memoria de CPU (I420: Y a resolución completa,
/// U y V a la mitad en cada eje).
#[derive(Debug)]
pub struct DecodedFrame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Bytes por fila de cada plano (`y`, `u`, `v`).
    pub strides: (usize, usize, usize),
    /// Ancho visible en píxeles.
    pub width: u32,
    /// Alto visible en píxeles.
    pub height: u32,
    /// Marca de tiempo de presentación en microsegundos (la de la entrada).
    pub pts_us: i64,
    /// Tiempo de decodificación en CPU.
    pub decode_time: Duration,
    /// Sin GPU de por medio: siempre `None` (se mantiene por compatibilidad
    /// con el backend de Windows).
    pub gpu_done_time: Option<Duration>,
}

/// Decodificador H.264 (OpenH264).
pub struct VideoDecoder {
    inner: Decoder,
    blob: PathBuf,
    size: (u32, u32),
    stream_changes: u32,
}

fn new_decoder(blob: &Path) -> Result<Decoder> {
    let api = OpenH264API::from_blob_path(blob)
        .map_err(|e| DecodeError::Unsupported(format!("cargando OpenH264: {e}")))?;
    Decoder::with_api_config(api, DecoderConfig::new())
        .map_err(|e| DecodeError::Unsupported(format!("creando el decodificador: {e}")))
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl VideoDecoder {
    pub fn new(ctx: &DecoderContext) -> Result<Self> {
        Self::new_with_size(ctx, None)
    }

    /// `initial_size` se acepta por compatibilidad con Windows; OpenH264 toma
    /// el tamaño del SPS.
    pub fn new_with_size(ctx: &DecoderContext, initial_size: Option<(u32, u32)>) -> Result<Self> {
        Ok(VideoDecoder {
            inner: new_decoder(&ctx.blob)?,
            blob: ctx.blob.clone(),
            size: initial_size.unwrap_or((0, 0)),
            stream_changes: 0,
        })
    }

    /// Sin efecto en este backend (no hay GPU que medir).
    pub fn set_measure_gpu(&mut self, _on: bool) {}

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Cambios de resolución observados.
    pub fn stream_changes(&self) -> u32 {
        self.stream_changes
    }

    /// Decodifica una *access unit* Annex-B. Sin B-frames, el frame sale en
    /// la misma llamada.
    pub fn decode(
        &mut self,
        annexb_access_unit: &[u8],
        pts_us: i64,
    ) -> Result<Option<DecodedFrame>> {
        let start = Instant::now();
        let yuv = match self.inner.decode(annexb_access_unit) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return Ok(None),
            Err(e) => return Err(DecodeError::Unexpected(format!("OpenH264: {e}"))),
        };
        let (w, h) = yuv.dimensions();
        let size = (w as u32, h as u32);
        if self.size != size {
            if self.size != (0, 0) {
                self.stream_changes += 1;
            }
            self.size = size;
        }
        let strides = yuv.strides();
        let frame = DecodedFrame {
            y: plane(yuv.y(), strides.0, h),
            u: plane(yuv.u(), strides.1, h.div_ceil(2)),
            v: plane(yuv.v(), strides.2, h.div_ceil(2)),
            strides,
            width: size.0,
            height: size.1,
            pts_us,
            decode_time: start.elapsed(),
            gpu_done_time: None,
        };
        Ok(Some(frame))
    }

    /// Frames pendientes (ninguno: la salida es síncrona).
    pub fn next_ready(&mut self) -> Option<DecodedFrame> {
        None
    }

    /// Vacía los frames retenidos al final del flujo.
    pub fn drain(&mut self) -> Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }

    /// Descarta el estado de referencias (tras una pérdida grave). OpenH264 no
    /// tiene un *flush* explícito: se recrea el decodificador desde el mismo
    /// binario. Si la recreación falla, se conserva el actual y se informa.
    pub fn flush(&mut self) -> Result<()> {
        self.inner = new_decoder(&self.blob)?;
        Ok(())
    }
}

/// Copia `rows` filas de `stride` bytes (sin pasarse del búfer de origen).
fn plane(src: &[u8], stride: usize, rows: usize) -> Vec<u8> {
    let len = (stride * rows).min(src.len());
    src[..len].to_vec()
}
