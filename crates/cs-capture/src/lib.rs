//! Captura de monitores en GPU (etapa 2 del pipeline, §2.4: ≤ 2 ms p95).
//!
//! La API es independiente del SO: un [`CaptureSource`] entrega
//! [`CapturedFrame`]s cuya imagen **sigue en la GPU** (el tipo de textura lo
//! fija cada backend) y mantiene aparte el estado del cursor
//! ([`CursorState`]), que viaja fuera de banda hacia el receptor.
//!
//! Backends:
//! - Windows: DXGI Desktop Duplication ([`windows_dxgi::DxgiCapture`]), con
//!   textura `ID3D11Texture2D` y el `ID3D11Device` expuesto para que el
//!   codificador trabaje sobre el mismo dispositivo y adaptador.
//! - Linux: captura experimental mediante XDG Portal/PipeWire (monitor duplicado).
//! - macOS: pendiente (ScreenCaptureKit).

pub mod bmp;
pub mod cursor;
pub mod stats;
pub mod yuv;

#[cfg(not(windows))]
pub mod linux_mutter;
#[cfg(all(not(windows), feature = "linux-pipewire"))]
pub mod linux_pipewire;
#[cfg(not(windows))]
pub mod linux_portal;
#[cfg(windows)]
pub mod windows_dxgi;

pub use cursor::{CursorShape, CursorShapeKind, CursorState};

use std::time::Duration;

/// Error de captura.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No existe (o no está activa en el escritorio) la salida pedida.
    #[error("no se encontró la salida de vídeo {0}")]
    OutputNotFound(String),
    /// La plataforma o el adaptador no soportan esta forma de captura
    /// (p. ej. Desktop Duplication sobre el adaptador equivocado en un
    /// portátil híbrido).
    #[error("captura no soportada: {0}")]
    Unsupported(String),
    /// Este proceso ya tiene una duplicación abierta de esa salida (DXGI
    /// solo permite una por proceso y salida; responde `E_INVALIDARG`).
    #[error("la salida {0} ya se está capturando en este proceso")]
    AlreadyCapturing(String),
    /// La salida desapareció y no volvió dentro del margen de recuperación.
    #[error("se perdió la salida y no se recuperó en {0:?}")]
    RecoveryTimeout(Duration),
    /// Fallo de una llamada del SO (HRESULT en Windows).
    #[error("{call} falló con código 0x{code:08X}")]
    Os { call: &'static str, code: u32 },
}

/// Formato de píxel de la textura capturada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits por canal, orden B,G,R,A (SDR, el caso normal).
    Bgra8,
    /// 16 bits flotantes por canal, R,G,B,A (escritorio HDR / color avanzado).
    Rgba16Float,
    /// Formato que el backend no reconoce (código nativo).
    Other(u32),
}

impl PixelFormat {
    /// Bytes por píxel, si se conoce.
    pub fn bytes_per_pixel(self) -> Option<u32> {
        match self {
            PixelFormat::Bgra8 => Some(4),
            PixelFormat::Rgba16Float => Some(8),
            PixelFormat::Other(_) => None,
        }
    }
}

/// Rectángulo en píxeles de la salida, semiabierto: `[left, right) × [top, bottom)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    /// Área en píxeles (0 si el rectángulo es degenerado).
    pub fn area(&self) -> u64 {
        self.width() as u64 * self.height() as u64
    }

    /// Intersección con otro rectángulo (degenerado si no se tocan).
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect {
            left: self.left.max(o.left),
            top: self.top.max(o.top),
            right: self.right.min(o.right),
            bottom: self.bottom.min(o.bottom),
        }
    }
}

/// Región copiada dentro del escritorio (p. ej. al arrastrar una ventana):
/// el contenido de `source_x, source_y` pasó a `destination`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRect {
    pub source_x: i32,
    pub source_y: i32,
    pub destination: Rect,
}

/// Qué cambió respecto al frame anterior entregado.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameUpdate {
    /// `true` si hay que tratar el frame como completamente nuevo: primer
    /// frame tras (re)crear la captura, o metadatos no disponibles.
    pub full: bool,
    /// Movimientos, a aplicar **antes** que las regiones sucias.
    pub moves: Vec<MoveRect>,
    /// Regiones sucias (pueden solaparse).
    pub dirty: Vec<Rect>,
}

impl FrameUpdate {
    /// Fracción de la pantalla `width × height` afectada (0..=1).
    ///
    /// Suma las áreas recortadas a la pantalla sin descontar solapes, así
    /// que es una **cota superior**; se satura en 1. Si `full`, vale 1.
    pub fn changed_fraction(&self, width: u32, height: u32) -> f64 {
        let total = width as u64 * height as u64;
        if self.full || total == 0 {
            return 1.0;
        }
        let screen = Rect {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let sum: u64 = self
            .dirty
            .iter()
            .chain(self.moves.iter().map(|m| &m.destination))
            .map(|r| r.intersect(&screen).area())
            .sum();
        (sum as f64 / total as f64).min(1.0)
    }
}

/// Marcas de tiempo de un frame, en microsegundos de un reloj monotónico del
/// host (QPC en Windows). Sirven para el presupuesto de latencia de §2.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTiming {
    /// Cuándo presentó el compositor este contenido (0 = desconocido).
    pub present_us: u64,
    /// Cuándo volvió la llamada de adquisición con el frame.
    pub acquired_us: u64,
    /// Cuándo la copia a la textura propia quedó lista: enviada a la GPU
    /// o, si se pidió esperar a la GPU, completada.
    pub ready_us: u64,
}

impl FrameTiming {
    /// Tiempo de captura propio: adquisición → copia lista.
    pub fn capture_us(&self) -> u64 {
        self.ready_us.saturating_sub(self.acquired_us)
    }

    /// Presentación del compositor → adquisición (None si no hay presentación).
    pub fn present_to_acquire_us(&self) -> Option<u64> {
        (self.present_us != 0).then(|| self.acquired_us.saturating_sub(self.present_us))
    }
}

/// Un frame capturado. `T` es la textura de GPU del backend.
#[derive(Debug, Clone)]
pub struct CapturedFrame<T> {
    /// Textura propia (no la del SO): sigue siendo válida tras devolver el
    /// frame al SO y mientras se conserve este valor.
    pub texture: T,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Número de secuencia de frames entregados (empieza en 1).
    pub sequence: u64,
    /// Presentaciones del compositor acumuladas en este frame (>1 = se
    /// perdieron frames intermedios porque no se adquirió a tiempo).
    pub accumulated_frames: u32,
    /// Contenido protegido (DRM) tapado por el SO.
    pub protected_content_masked: bool,
    pub timing: FrameTiming,
    pub update: FrameUpdate,
    /// Cambia cuando el backend tuvo que recrear el dispositivo de GPU (p. ej.
    /// la salida pasó a otro adaptador): el consumidor debe reabrir recursos.
    pub device_generation: u32,
}

/// Contadores acumulados de una fuente de captura.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureStats {
    /// Llamadas a `next_frame`.
    pub calls: u64,
    /// Frames con imagen entregados.
    pub frames: u64,
    /// Llamadas que terminaron sin frame nuevo (escritorio estático).
    pub timeouts: u64,
    /// Actualizaciones solo de cursor (sin imagen nueva).
    pub pointer_only_updates: u64,
    /// Veces que el SO invalidó la captura (cambio de modo, topología,
    /// escritorio seguro...).
    pub access_lost: u64,
    /// Recreaciones correctas de la captura.
    pub reinits: u64,
    /// Intentos de recreación fallidos (la salida seguía en transición).
    pub failed_reinits: u64,
    /// Último error de un intento de recreación fallido.
    pub last_reinit_error: Option<String>,
    /// Tiempo desde la última pérdida hasta tener la captura recreada.
    pub last_reinit_latency: Option<Duration>,
    /// Recreaciones del dispositivo de GPU.
    pub device_recreations: u64,
    /// Tiempo desde la última pérdida hasta el primer frame posterior.
    pub last_recovery: Option<Duration>,
}

/// Información de una salida de vídeo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// Nombre GDI, p. ej. `\\.\DISPLAY12`.
    pub gdi_name: String,
    /// Descripción del adaptador que la expone (p. ej. "Intel(R) Iris(R) Xe Graphics").
    pub adapter_name: String,
    /// LUID del adaptador (identifica la GPU para el codificador).
    pub adapter_luid: i64,
    /// Coordenadas en el escritorio virtual (píxeles físicos).
    pub desktop_rect: Rect,
    pub attached_to_desktop: bool,
}

/// Qué salida capturar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    /// Por nombre GDI (`\\.\DISPLAYn`), comparado sin distinguir mayúsculas.
    GdiName(String),
    /// La salida principal (la que tiene el origen del escritorio en 0,0).
    Primary,
}

impl CaptureTarget {
    /// ¿Corresponde esta salida al objetivo?
    pub fn matches(&self, out: &OutputInfo) -> bool {
        match self {
            CaptureTarget::GdiName(n) => gdi_name_eq(n, &out.gdi_name),
            CaptureTarget::Primary => {
                out.attached_to_desktop && out.desktop_rect.left == 0 && out.desktop_rect.top == 0
            }
        }
    }
}

impl std::fmt::Display for CaptureTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureTarget::GdiName(n) => f.write_str(n),
            CaptureTarget::Primary => f.write_str("<principal>"),
        }
    }
}

/// Compara nombres GDI ignorando mayúsculas, espacios y NUL finales.
pub fn gdi_name_eq(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['\0', ' ']).trim().to_ascii_uppercase();
    norm(a) == norm(b)
}

/// Convierte un buffer UTF-16 terminado en NUL (típico de Win32) a `String`.
pub fn utf16_z_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Convierte ticks de un contador de frecuencia `freq` Hz a microsegundos
/// sin desbordar (`ticks * 1e6` desborda i64 tras ~10 días a 10 MHz).
pub fn ticks_to_us(ticks: i64, freq: i64) -> u64 {
    if ticks <= 0 || freq <= 0 {
        return 0;
    }
    let (t, f) = (ticks as u128, freq as u128);
    (t * 1_000_000 / f) as u64
}

/// Configuración de una fuente de captura.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Recoger regiones sucias y movimientos (coste pequeño en CPU).
    pub collect_metadata: bool,
    /// Esperar a que la GPU termine la copia antes de devolver el frame.
    /// No hace falta si el consumidor usa el mismo contexto D3D11 (el orden
    /// lo garantiza la cola de la GPU); sirve para medir la copia real.
    pub wait_for_gpu_copy: bool,
    /// Cuánto se tolera que la salida falte tras una pérdida (cambio de
    /// modo, topología) antes de devolver [`CaptureError::RecoveryTimeout`].
    pub recovery_grace: Duration,
    /// Máximo de texturas propias en circulación a la vez.
    pub max_pool_textures: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            collect_metadata: true,
            wait_for_gpu_copy: false,
            recovery_grace: Duration::from_secs(5),
            max_pool_textures: 4,
        }
    }
}

/// Fuente de frames de un monitor.
pub trait CaptureSource {
    /// Textura de GPU del backend.
    type Texture;

    /// Espera hasta `timeout` un frame con imagen nueva.
    ///
    /// - `Ok(Some(frame))`: imagen nueva (copiada a una textura propia).
    /// - `Ok(None)`: no cambió nada (escritorio estático) o la captura se
    ///   está recuperando de una pérdida; los cambios solo de cursor se
    ///   absorben en [`CaptureSource::cursor`].
    /// - `Err(_)`: error no recuperable.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame<Self::Texture>>, CaptureError>;

    /// Último estado conocido del cursor (fuera de banda).
    fn cursor(&self) -> &CursorState;

    /// Salida que se está capturando.
    fn output(&self) -> &OutputInfo;

    /// Contadores acumulados.
    fn stats(&self) -> &CaptureStats;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_area_and_intersection() {
        let a = Rect {
            left: 0,
            top: 0,
            right: 10,
            bottom: 10,
        };
        let b = Rect {
            left: 5,
            top: 5,
            right: 20,
            bottom: 20,
        };
        assert_eq!(a.area(), 100);
        assert_eq!(a.intersect(&b).area(), 25);
        let far = Rect {
            left: 50,
            top: 50,
            right: 60,
            bottom: 60,
        };
        assert_eq!(a.intersect(&far).area(), 0);
    }

    #[test]
    fn changed_fraction_clips_and_saturates() {
        let mut u = FrameUpdate {
            full: false,
            moves: vec![],
            dirty: vec![Rect {
                left: -10,
                top: 0,
                right: 50,
                bottom: 100,
            }],
        };
        // Recortado a 0..50 × 0..100 = 5000 de 100×100.
        assert!((u.changed_fraction(100, 100) - 0.5).abs() < 1e-9);
        u.dirty.push(Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        });
        assert_eq!(u.changed_fraction(100, 100), 1.0);
        u.dirty.clear();
        assert_eq!(u.changed_fraction(100, 100), 0.0);
        u.full = true;
        assert_eq!(u.changed_fraction(100, 100), 1.0);
    }

    #[test]
    fn gdi_names_compare_loosely() {
        assert!(gdi_name_eq(r"\\.\DISPLAY12", r"\\.\display12"));
        assert!(gdi_name_eq(r"\\.\DISPLAY12", "\\\\.\\DISPLAY12\0\0"));
        assert!(!gdi_name_eq(r"\\.\DISPLAY1", r"\\.\DISPLAY12"));
    }

    #[test]
    fn utf16_conversion_stops_at_nul() {
        let mut buf = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY1".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(utf16_z_to_string(&buf), r"\\.\DISPLAY1");
        assert_eq!(utf16_z_to_string(&[]), "");
    }

    #[test]
    fn ticks_conversion_does_not_overflow() {
        assert_eq!(ticks_to_us(10_000_000, 10_000_000), 1_000_000);
        assert_eq!(ticks_to_us(0, 10_000_000), 0);
        assert_eq!(ticks_to_us(5, 0), 0);
        // 30 días a 10 MHz: ticks*1e6 desbordaría i64.
        let ticks = 30i64 * 86_400 * 10_000_000;
        assert_eq!(ticks_to_us(ticks, 10_000_000), 30 * 86_400 * 1_000_000);
    }

    #[test]
    fn primary_target_matches_origin() {
        let mut out = OutputInfo {
            gdi_name: r"\\.\DISPLAY1".into(),
            adapter_name: "x".into(),
            adapter_luid: 1,
            desktop_rect: Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            attached_to_desktop: true,
        };
        assert!(CaptureTarget::Primary.matches(&out));
        assert!(CaptureTarget::GdiName(r"\\.\display1".into()).matches(&out));
        out.desktop_rect.left = 1920;
        assert!(!CaptureTarget::Primary.matches(&out));
    }

    #[test]
    fn timing_helpers() {
        let t = FrameTiming {
            present_us: 100,
            acquired_us: 150,
            ready_us: 400,
        };
        assert_eq!(t.capture_us(), 250);
        assert_eq!(t.present_to_acquire_us(), Some(50));
        let t0 = FrameTiming { present_us: 0, ..t };
        assert_eq!(t0.present_to_acquire_us(), None);
    }
}
