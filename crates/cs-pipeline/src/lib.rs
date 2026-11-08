//! Pipeline **emisor** de CrossScreen (F2, MVP).
//!
//! monitor virtual (`cs-display`) → captura en GPU (`cs-capture`, DXGI) →
//! H.264 por hardware sobre el mismo `ID3D11Device` (`cs-encode`, MF) →
//! datagramas QUIC con FEC y *pacing* + canal de control (`cs-transport`).
//!
//! Implementa el lado fuente del contrato `docs/09-contrato-sesion-mvp.md`.
//! La CLI (`crossscreen stream`) es una envoltura fina sobre
//! [`run_sender`]; `crossscreen-agent` podrá reutilizarlo pasando su propia
//! conexión y su señal de parada.
//!
//! Diseño y medidas: `docs/poc-results/mvp-f2-emisor.md`.

pub mod cursor;
pub mod display_guard;
pub mod stats;
pub mod timing;

#[cfg(windows)]
mod session;
#[cfg(windows)]
pub mod win;

pub use stats::SenderReport;

use cs_core::{DeviceId, Mode, StreamMode};
use cs_security::Fingerprint;
use cs_transport::simloss::LossModel;
use cs_transport::{Endpoint, PacingMode, WaitStrategy};
use std::net::SocketAddr;
use std::time::Duration;

/// `stream_id` único del MVP.
pub const STREAM_ID: u16 = 1;

/// Ventana de gracia por defecto para reconectar sin retirar el monitor virtual.
pub const DEFAULT_RECONNECT_GRACE: Duration = Duration::from_secs(15);

/// Parámetros de reconexión automática ante cortes temporales de red.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    pub endpoint: Endpoint,
    pub addr: SocketAddr,
    pub peer: Fingerprint,
    /// Tiempo máximo intentando reconectar antes de retirar el monitor virtual.
    pub grace: Duration,
}

/// Configuración de una sesión emisora.
#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub device_id: DeviceId,
    pub device_name: String,
    /// `Extend`: monitor virtual nuevo. `Duplicate`: la pantalla principal.
    pub mode: StreamMode,
    /// Modo pedido explícitamente (CLI). `None`: el `native_mode` del
    /// receptor, con [`SenderConfig::fallback_mode`] como reserva.
    pub requested_mode: Option<Mode>,
    pub fallback_mode: Mode,
    pub bitrate_kbps: u32,
    /// Paridad inicial de los P-frames (o fija si `adaptive_fec = false`).
    pub fec_ratio: f32,
    /// Paridad de los IDR (se usa `max(keyframe_fec_ratio, fec_ratio)`).
    pub keyframe_fec_ratio: f32,
    /// Entrelazar los shards entre bloques FEC de un mismo frame.
    pub fec_interleave: bool,
    /// Ajustar la paridad de los P-frames según `ReceiverStats`.
    pub adaptive_fec: bool,
    /// Valor opcional para `CODECAPI_AVEncVideoGradualIntraRefresh`.
    pub intra_refresh: Option<u32>,
    pub pacing: PacingMode,
    pub pacing_wait: WaitStrategy,
    /// Duración máxima (`--seconds`).
    pub duration: Option<Duration>,
    /// Con el escritorio estático, recodificar la última imagen cada este
    /// intervalo (`None` = nunca). Ver el módulo `win::worker`.
    pub static_refresh: Option<Duration>,
    /// Si `Some`, ante pérdida de conexión QUIC intenta reconectar durante
    /// `grace` conservando el monitor virtual y el hilo de captura.
    pub reconnect: Option<ReconnectConfig>,
    /// Permitir que el receptor inyecte eventos de ratón y teclado (F6) en el
    /// rectángulo del monitor transmitido.
    pub accept_input: bool,
    /// Sincronizar el portapapeles de texto con el receptor (F7), en ambos
    /// sentidos. Solo los cambios posteriores al inicio de la sesión.
    pub clipboard: bool,
    /// **Solo pruebas**: inyectar pérdidas en los datagramas salientes
    /// `(modelo, semilla)`.
    pub sim_loss: Option<(LossModel, u64)>,
}

impl SenderConfig {
    /// Valores por defecto: 20 Mbps, FEC 0,15 (IDR 0,5, entrelazado y
    /// adaptativo), *pacing* 0,6.
    pub fn new(device_id: DeviceId, device_name: String) -> Self {
        Self {
            device_id,
            device_name,
            mode: StreamMode::Extend,
            requested_mode: None,
            fallback_mode: Mode::new(1920, 1080, 60),
            bitrate_kbps: 20_000,
            fec_ratio: 0.15,
            keyframe_fec_ratio: 0.5,
            fec_interleave: true,
            adaptive_fec: true,
            intra_refresh: None,
            pacing: PacingMode::Spread { fraction: 0.6 },
            pacing_wait: WaitStrategy::HighResTimer,
            duration: None,
            static_refresh: Some(Duration::from_secs(1)),
            reconnect: None,
            accept_input: true,
            clipboard: true,
            sim_loss: None,
        }
    }
}

/// Ejecuta una sesión emisora completa sobre `conn` (ya autenticada contra
/// el receptor) hasta que termine: `shutdown` pasa a `true`, se cumple la
/// duración, el receptor se despide o se pierde la conexión.
///
/// El monitor virtual se retira **siempre** antes de volver (también con
/// error). Devuelve el informe de métricas; un fallo de captura o
/// codificación a mitad de sesión se refleja en `report.worker.fatal`.
#[cfg(windows)]
pub async fn run_sender(
    conn: cs_transport::Connection,
    cfg: SenderConfig,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<SenderReport> {
    session::run(conn, cfg, shutdown).await
}

/// Sin backend de captura/codificación fuera de Windows (todavía).
#[cfg(not(windows))]
pub async fn run_sender(
    _conn: cs_transport::Connection,
    _cfg: SenderConfig,
    _shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<SenderReport> {
    anyhow::bail!("el emisor solo está implementado en Windows por ahora")
}
