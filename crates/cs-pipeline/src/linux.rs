//! Emisor Linux: XDG Desktop Portal → PipeWire → OpenH264 → QUIC.
//!
//! Duplica el monitor que el usuario elige en el diálogo del sistema (no
//! crea una pantalla virtual: `extend` se trata como `duplicate`).
//!
//! Se hace en dos pasos para que el diálogo nunca choque con los
//! temporizadores del receptor (10 s para el canal de control y para
//! `StartStream`):
//! 1. [`prepare`], **antes de conectar**: comprueba OpenH264, abre el diálogo
//!    del portal, arranca la captura y espera el primer frame (tamaño real).
//! 2. [`run`]: handshake, `StartStream` y transmisión.
//!
//! Siempre se libera todo al terminar (también con error): hilo de
//! captura/codificación, PipeWire y la sesión del portal (si no se cierra,
//! GNOME mantiene el indicador de "compartiendo pantalla").

use crate::stats::{ControlStats, SenderReport, WorkerStats};
use crate::timing::wire_pts;
use crate::{SenderConfig, STREAM_ID};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use cs_capture::linux_input::InputAction;
use cs_capture::linux_mutter::MutterVirtual;
use cs_capture::linux_pipewire::I420Buffer;
use cs_capture::linux_portal::{describe_error, PortalCaptureSession, PortalSession};
use cs_capture::linux_sway::SwayVirtualOutput;
use cs_core::{Codec, Mode, Roles, StreamMode};
use cs_encode::openh264_backend::{I420Frame, OpenH264Encoder};
use cs_encode::{EncodeControl, EncoderConfig, VideoEncoder};
use cs_input::evdev;
use cs_input::{ButtonState, InputEvent, PressedState};
use cs_protocol::control::{ByeReason, ControlMessage, Hello, SinkCapabilities, StartStream};
use cs_protocol::PROTOCOL_VERSION;
use cs_transport::pacer::{self, PacedSendOptions, Pacer, PacerConfig};
use cs_transport::packetizer::FrameMeta;
use cs_transport::{Connection, FramedStream, Packetizer, PacketizerConfig};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc as tokio_mpsc, watch};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Tiempo para responder al diálogo de compartir pantalla (la petición IPC
/// del panel espera hasta 180 s).
const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Espera máxima al primer frame de PipeWire (tamaño real de la captura).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Intervalo mínimo entre dos IDR forzados por `RequestKeyframe`.
const MIN_IDR_INTERVAL: Duration = Duration::from_millis(250);
/// Con la pantalla quieta el compositor no entrega frames: se vuelve a
/// codificar el último con esta cadencia (latido para el receptor y para
/// que un IDR pedido salga aunque nada cambie).
const IDLE_REPEAT: Duration = Duration::from_secs(1);
/// Diagnóstico: consumir este nodo del PipeWire local sin pasar por el
/// portal (pruebas sin GNOME, p. ej. con una fuente de vídeo sintética).
pub const TEST_NODE_ENV: &str = "CROSSSCREEN_TEST_PIPEWIRE_NODE";

struct LinuxUnit {
    data: Vec<u8>,
    keyframe: bool,
    pts_us: u64,
}

/// Estado compartido entre la sesión (async) y el hilo de codificación.
struct Shared {
    /// Hasta que el receptor confirma `StreamStarted` no se codifica: los
    /// frames solo sirven para conocer el tamaño real de la captura.
    streaming: AtomicBool,
    /// El próximo frame debe salir como IDR.
    force_idr: AtomicBool,
    stop: AtomicBool,
}

struct LinuxWorker {
    shared: Arc<Shared>,
    join: Option<std::thread::JoinHandle<WorkerStats>>,
    /// Control de la captura (PipeWire o wlr-screencopy): al soltarlo se para.
    capture_control: Option<CaptureControl>,
}

/// Control de la captura en marcha; al soltarlo se detiene su hilo.
#[allow(dead_code)] // solo se usa por su `Drop`
enum CaptureControl {
    PipeWire(cs_capture::linux_pipewire::PipeWireCaptureControl),
    Wlr(cs_capture::linux_wlr::WlrCaptureControl),
}

/// De dónde salen los frames.
enum FrameSource {
    /// Nodo PipeWire (portal, Mutter o nodo de prueba).
    PipeWire {
        node: u32,
        fd: OwnedFd,
        fixed_size: Option<(u32, u32)>,
    },
    /// Salida de un compositor wlroots por `wlr-screencopy` (Sway).
    Wlr { output: String, fps: u32 },
}

impl LinuxWorker {
    fn start(
        source: FrameSource,
        cfg: &SenderConfig,
        tx: tokio_mpsc::Sender<LinuxUnit>,
        size_tx: watch::Sender<Option<(u32, u32)>>,
    ) -> Result<Self> {
        let (frames, capture_control): (mpsc::Receiver<I420Buffer>, CaptureControl) = match source {
            FrameSource::PipeWire {
                node,
                fd,
                fixed_size,
            } => {
                let capture = cs_capture::linux_pipewire::start_i420_sized(node, fd, fixed_size)
                    .context("iniciando el stream PipeWire")?;
                let (frames, control) = capture.into_parts();
                (frames, CaptureControl::PipeWire(control))
            }
            FrameSource::Wlr { output, fps } => {
                let (frames, control) = cs_capture::linux_wlr::start_i420(&output, fps)
                    .context("iniciando la captura wlr-screencopy")?;
                (frames, CaptureControl::Wlr(control))
            }
        };
        let shared = Arc::new(Shared {
            streaming: AtomicBool::new(false),
            force_idr: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let thread_shared = Arc::clone(&shared);
        let bitrate = cfg.bitrate_kbps;
        let join = std::thread::Builder::new()
            .name("crossscreen-linux-capture-encode".into())
            .spawn(move || encode_loop(frames, thread_shared, bitrate, tx, size_tx))
            .context("creando el hilo Linux de captura")?;
        Ok(Self {
            shared,
            join: Some(join),
            capture_control: Some(capture_control),
        })
    }

    fn finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    async fn stop(mut self) -> WorkerStats {
        self.shared.stop.store(true, Ordering::Relaxed);
        let result = match self.join.take() {
            Some(join) => tokio::task::spawn_blocking(move || {
                join.join().unwrap_or_else(|_| WorkerStats {
                    fatal: Some("el hilo Linux de captura terminó con pánico".into()),
                    ..WorkerStats::default()
                })
            })
            .await
            .unwrap_or_else(|_| WorkerStats {
                fatal: Some("no se pudo esperar al hilo Linux de captura".into()),
                ..WorkerStats::default()
            }),
            None => WorkerStats::default(),
        };
        if let Some(control) = self.capture_control.take() {
            // Detener la captura bloquea brevemente (join de su hilo).
            let _ = tokio::task::spawn_blocking(move || drop(control)).await;
        }
        result
    }
}

/// Bucle del hilo de codificación: recibe frames I420 de PipeWire, los
/// codifica con OpenH264 cuando la sesión ya transmite y repite el último
/// frame si la pantalla está quieta.
fn encode_loop(
    frames: mpsc::Receiver<I420Buffer>,
    shared: Arc<Shared>,
    bitrate_kbps: u32,
    tx: tokio_mpsc::Sender<LinuxUnit>,
    size_tx: watch::Sender<Option<(u32, u32)>>,
) -> WorkerStats {
    let mut stats = WorkerStats::default();
    let mut encoder: Option<(OpenH264Encoder, (u32, u32))> = None;
    let mut last_frame: Option<I420Frame> = None;
    let mut last_encode = Instant::now();
    let mut epoch: Option<Instant> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        let frame = match frames.recv_timeout(Duration::from_millis(100)) {
            Ok(buffer) => {
                stats.captured += 1;
                // H.264 4:2:0 necesita dimensiones pares: con escalado
                // fraccional la captura puede ser impar; se recorta 1 px (el
                // stride no cambia).
                let (width, height) = (buffer.width & !1, buffer.height & !1);
                if width == 0 || height == 0 {
                    continue;
                }
                stats.last_size = Some((width, height));
                size_tx.send_if_modified(|old| {
                    let changed = *old != Some((width, height));
                    *old = Some((width, height));
                    changed
                });
                let frame = match I420Frame::new(
                    width,
                    height,
                    buffer.y,
                    buffer.u,
                    buffer.v,
                    buffer.strides,
                ) {
                    Ok(frame) => frame,
                    Err(error) => {
                        stats.encoder_errors += 1;
                        tracing::warn!("frame I420 inválido: {error}");
                        continue;
                    }
                };
                if !shared.streaming.load(Ordering::Relaxed) {
                    // Aún no se transmite: guardar el más reciente. Con la
                    // pantalla quieta (GNOME solo envía frames cuando algo
                    // cambia) será el primero que se codifique, como IDR.
                    last_frame = Some(frame);
                    continue;
                }
                frame
            }
            Err(RecvTimeoutError::Timeout) => {
                // Pantalla quieta: repetir el último frame si hace falta un
                // IDR o como latido.
                let due = shared.force_idr.load(Ordering::Relaxed)
                    || last_encode.elapsed() >= IDLE_REPEAT;
                match last_frame.take() {
                    Some(frame) if due && shared.streaming.load(Ordering::Relaxed) => {
                        stats.repeats += 1;
                        frame
                    }
                    other => {
                        last_frame = other;
                        continue;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // El hilo de PipeWire terminó: el usuario dejó de compartir
                // o el compositor cerró el stream. No es un fallo.
                tracing::info!("la captura de PipeWire terminó");
                break;
            }
        };
        let size = (frame.width, frame.height);
        if encoder.as_ref().is_none_or(|(_, s)| *s != size) {
            let config = EncoderConfig::desktop(size.0, size.1, 60, bitrate_kbps);
            match OpenH264Encoder::from_default_path(&config) {
                Ok(enc) => {
                    tracing::info!("codificador OpenH264 creado: {}x{}", size.0, size.1);
                    encoder = Some((enc, size));
                    stats.encoders_created += 1;
                    shared.force_idr.store(true, Ordering::Relaxed);
                }
                Err(error) => {
                    tracing::error!("no se pudo crear el codificador OpenH264: {error}");
                    stats.fatal = Some(error.to_string());
                    break;
                }
            }
        }
        let Some((enc, _)) = encoder.as_mut() else {
            continue;
        };
        let force_idr = shared.force_idr.swap(false, Ordering::Relaxed);
        stats.forced_idr += u64::from(force_idr);
        let units = match enc.encode(
            &frame,
            EncodeControl {
                force_idr,
                ..Default::default()
            },
        ) {
            Ok(units) => units,
            Err(error) => {
                stats.encoder_errors += 1;
                tracing::warn!("fallo codificando OpenH264: {error}");
                shared.force_idr.store(true, Ordering::Relaxed);
                continue;
            }
        };
        last_encode = Instant::now();
        // Marca de tiempo real desde el primer frame transmitido.
        let pts_us = epoch.get_or_insert(last_encode).elapsed().as_micros() as u64;
        last_frame = Some(frame);
        for unit in units {
            stats.encoded += 1;
            stats.encode_us.push(unit.encode_time.as_micros() as f64);
            stats.idr_out += u64::from(unit.keyframe);
            if unit.keyframe {
                stats.idr_bytes += unit.data.len() as u64;
            } else {
                stats.p_bytes += unit.data.len() as u64;
            }
            let item = LinuxUnit {
                data: unit.data,
                keyframe: unit.keyframe,
                pts_us,
            };
            match tx.try_send(item) {
                Ok(()) => {}
                Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                    // Un frame perdido rompe la cadena de referencias: el
                    // siguiente debe ser IDR o el receptor no podrá decodificar.
                    stats.dropped_backpressure += 1;
                    shared.force_idr.store(true, Ordering::Relaxed);
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => return stats,
            }
        }
    }
    stats
}

/// Sesión del sistema que hay que cerrar al terminar.
enum SystemSession {
    /// Duplicar: sesión del portal (diálogo de GNOME).
    Portal(PortalSession),
    /// Extender: sesión de Mutter con el monitor virtual.
    Mutter(MutterVirtual),
    /// Extender en Sway: salida virtual *headless* (ADR-0011).
    Sway(SwayVirtualOutput),
}

impl SystemSession {
    async fn close(self) {
        match self {
            SystemSession::Portal(p) => p.close().await,
            SystemSession::Mutter(m) => m.close().await,
            SystemSession::Sway(o) => {
                let _ = tokio::task::spawn_blocking(move || o.remove()).await;
            }
        }
    }

    async fn wait_closed(&self) {
        match self {
            SystemSession::Portal(p) => p.wait_closed().await,
            SystemSession::Mutter(m) => m.wait_closed().await,
            // Si el usuario retira la salida (p. ej. `swaymsg output … unplug`).
            SystemSession::Sway(o) => {
                let name = o.name().to_owned();
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let n = name.clone();
                    let exists = tokio::task::spawn_blocking(move || {
                        cs_capture::linux_sway::output_exists(&n)
                    })
                    .await
                    .unwrap_or(true);
                    if !exists {
                        tracing::info!(output = %name, "la salida virtual de Sway se retiró");
                        return;
                    }
                }
            }
        }
    }

    /// ¿Se puede inyectar ratón y teclado en esta sesión?
    fn can_inject(&self) -> bool {
        match self {
            SystemSession::Portal(p) => p.can_inject(),
            SystemSession::Mutter(m) => m.can_inject(),
            SystemSession::Sway(_) => false,
        }
    }

    async fn inject(&self, action: &InputAction) -> Result<(), String> {
        match self {
            SystemSession::Portal(p) => p.inject(action).await.map_err(|e| e.to_string()),
            SystemSession::Mutter(m) => m.inject(action).await.map_err(|e| e.to_string()),
            SystemSession::Sway(_) => Err("control remoto aún no disponible en Sway".into()),
        }
    }
}

/// Traduce un evento del protocolo a acciones de GNOME (códigos evdev y
/// píxeles del stream de tamaño `size`).
fn to_actions(
    event: &InputEvent,
    size: (u32, u32),
    wheel: &mut evdev::WheelAccumulator,
) -> Vec<InputAction> {
    match *event {
        InputEvent::MouseMove { x_norm, y_norm, .. } => vec![InputAction::PointerAbs {
            x: evdev::denormalize(x_norm, size.0),
            y: evdev::denormalize(y_norm, size.1),
        }],
        InputEvent::MouseButton { button, state, .. } => vec![InputAction::Button {
            code: evdev::button(button),
            pressed: state == ButtonState::Pressed,
        }],
        InputEvent::MouseWheel {
            delta_x, delta_y, ..
        } => {
            let (sx, sy) = wheel.push(delta_x, delta_y);
            let mut v = Vec::new();
            if sy != 0 {
                v.push(InputAction::Axis {
                    horizontal: false,
                    steps: sy,
                });
            }
            if sx != 0 {
                v.push(InputAction::Axis {
                    horizontal: true,
                    steps: sx,
                });
            }
            v
        }
        InputEvent::Key {
            scancode,
            extended,
            state,
            ..
        } => evdev::keycode(scancode, extended)
            .map(|code| InputAction::Key {
                code,
                pressed: state == ButtonState::Pressed,
            })
            .into_iter()
            .collect(),
    }
}

/// Captura en marcha: hilo de codificación y sus canales.
struct Capture {
    worker: LinuxWorker,
    video_rx: Option<tokio_mpsc::Receiver<LinuxUnit>>,
    size_rx: watch::Receiver<Option<(u32, u32)>>,
}

/// Fuente Linux preparada antes de conectar con el receptor.
///
/// - Duplicar: el usuario ya eligió la pantalla en el diálogo y la captura
///   tiene su primer frame.
/// - Extender: solo se comprobó que hay GNOME; el monitor virtual se crea en
///   la sesión, cuando se conoce la resolución del receptor (sin diálogo).
///
/// Si se suelta sin terminar la sesión (p. ej. falla la conexión), `Drop`
/// detiene la captura y cierra la sesión del sistema.
pub(crate) struct Prepared {
    system: Option<SystemSession>,
    capture: Option<Capture>,
    /// Extender con un monitor que se crea en la sesión (GNOME o Sway), ya
    /// con la resolución del receptor.
    extend: Option<VirtualBackend>,
    /// La captura ya preparada es de un monitor virtual (portal `VIRTUAL`,
    /// KDE Plasma): la sesión es de extender aunque no haya que crearlo.
    portal_virtual: bool,
}

/// Cómo se crea el monitor virtual al conocer la resolución del receptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtualBackend {
    /// GNOME: API de Mutter (`RecordVirtual`).
    Mutter,
    /// Sway: `swaymsg create_output` + `wlr-screencopy`.
    Sway,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(c) = self.capture.take() {
            // El hilo sale en ≤100 ms; soltar el control detiene PipeWire.
            c.worker.shared.stop.store(true, Ordering::Relaxed);
        }
        if let Some(system) = self.system.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(system.close());
                }
                Err(_) => tracing::warn!("sesión del sistema sin cerrar (sin runtime)"),
            }
        }
    }
}

impl Prepared {
    /// Libera todo: hilo de codificación, PipeWire y la sesión del sistema
    /// (portal o monitor virtual).
    async fn release(&mut self) -> WorkerStats {
        let stats = match self.capture.take() {
            Some(c) => c.worker.stop().await,
            None => WorkerStats::default(),
        };
        if let Some(system) = self.system.take() {
            system.close().await;
        }
        stats
    }
}

/// Socket del PipeWire local de este usuario (`PIPEWIRE_REMOTE` o
/// `$XDG_RUNTIME_DIR/pipewire-0`).
fn local_pipewire() -> Result<OwnedFd> {
    let socket = match std::env::var_os("PIPEWIRE_REMOTE") {
        Some(path) => std::path::PathBuf::from(path),
        None => std::path::PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").context("falta XDG_RUNTIME_DIR")?,
        )
        .join("pipewire-0"),
    };
    let stream = std::os::unix::net::UnixStream::connect(&socket)
        .with_context(|| format!("conectando con PipeWire en {}", socket.display()))?;
    Ok(stream.into())
}

/// Nodo de prueba de [`TEST_NODE_ENV`] y conexión al PipeWire local.
fn test_source() -> Result<Option<(u32, OwnedFd)>> {
    let Some(node) = std::env::var_os(TEST_NODE_ENV) else {
        return Ok(None);
    };
    let node: u32 = node
        .to_str()
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("{TEST_NODE_ENV} debe ser un id de nodo"))?;
    tracing::warn!("{TEST_NODE_ENV}={node}: se usa el nodo PipeWire local sin portal");
    Ok(Some((node, local_pipewire()?)))
}

/// Arranca la captura de `node_id` y espera su primer frame.
async fn start_capture(source: FrameSource, cfg: &SenderConfig) -> Result<Capture> {
    let (video_tx, video_rx) = tokio_mpsc::channel(4);
    let (size_tx, mut size_rx) = watch::channel(None);
    let worker = LinuxWorker::start(source, cfg, video_tx, size_tx)?;
    let first = tokio::time::timeout(FIRST_FRAME_TIMEOUT, size_rx.wait_for(Option::is_some))
        .await
        .ok()
        .and_then(|r| r.ok().and_then(|v| *v));
    let Some((width, height)) = first else {
        let stats = worker.stop().await;
        bail!(
            "la captura no entregó ningún frame en {} s (capturados: {}){}",
            FIRST_FRAME_TIMEOUT.as_secs(),
            stats.captured,
            stats.fatal.map(|f| format!("; {f}")).unwrap_or_default()
        );
    };
    tracing::info!("captura lista: {width}x{height}");
    Ok(Capture {
        worker,
        video_rx: Some(video_rx),
        size_rx,
    })
}

/// Paso 1 (antes de conectar con el receptor): comprueba el codificador y,
/// para duplicar, pide la pantalla al usuario y arranca la captura.
pub(crate) async fn prepare(cfg: &SenderConfig) -> Result<Prepared> {
    // OpenH264 primero: si falta, no tiene sentido seguir.
    OpenH264Encoder::from_default_path(&EncoderConfig::desktop(640, 480, 60, 1000)).map_err(
        |e| anyhow!("{e} (con el .deb el script está en /usr/lib/CrossScreen/fetch-openh264.sh)"),
    )?;

    if let Some((node, fd)) = test_source()? {
        let capture = start_capture(
            FrameSource::PipeWire {
                node,
                fd,
                fixed_size: None,
            },
            cfg,
        )
        .await?;
        return Ok(Prepared {
            system: None,
            capture: Some(capture),
            extend: None,
            portal_virtual: false,
        });
    }

    let mut portal_virtual = false;
    if cfg.mode == StreamMode::Extend {
        // Monitor virtual, por orden de preferencia (ADR-0007, ADR-0011):
        // GNOME y Sway lo crean con la resolución del receptor y sin
        // diálogo; si no, el portal con fuente VIRTUAL (KDE Plasma).
        if MutterVirtual::is_available().await {
            return Ok(Prepared {
                system: None,
                capture: None,
                extend: Some(VirtualBackend::Mutter),
                portal_virtual: false,
            });
        }
        let sway = tokio::task::spawn_blocking(|| {
            cs_capture::linux_sway::is_available() && cs_capture::linux_wlr::is_available()
        })
        .await
        .unwrap_or(false);
        if sway {
            if cfg.accept_input {
                tracing::warn!(
                    "control remoto aún no disponible en Sway: solo se transmite la imagen"
                );
            }
            return Ok(Prepared {
                system: None,
                capture: None,
                extend: Some(VirtualBackend::Sway),
                portal_virtual: false,
            });
        }
        if !cs_capture::linux_portal::supports_virtual_monitor().await {
            bail!(
                "este escritorio no permite crear un monitor virtual. Extender funciona en GNOME, KDE Plasma (5.27 o posterior) y Sway; en los demás usa «duplicar»"
            );
        }
        portal_virtual = true;
    }

    tracing::info!(
        virtual_monitor = portal_virtual,
        "pidiendo la pantalla al XDG Desktop Portal (diálogo del sistema)"
    );
    let opened = tokio::time::timeout(
        PORTAL_TIMEOUT,
        PortalCaptureSession::open_source(cfg.accept_input, portal_virtual),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "no se respondió al diálogo de compartir pantalla en {} s",
            PORTAL_TIMEOUT.as_secs()
        )
    })?
    .map_err(|e| anyhow!("{}", describe_error(&e)))?;
    let PortalCaptureSession {
        stream,
        pipewire_fd,
        session,
    } = opened;
    tracing::info!(
        node = stream.node_id,
        size = ?stream.size,
        "el portal autorizó la captura"
    );
    let mut prepared = Prepared {
        system: Some(SystemSession::Portal(session)),
        capture: None,
        extend: None,
        portal_virtual,
    };
    // Si falla, `prepared` se suelta y cierra la sesión del portal.
    prepared.capture = Some(
        start_capture(
            FrameSource::PipeWire {
                node: stream.node_id,
                fd: pipewire_fd,
                fixed_size: None,
            },
            cfg,
        )
        .await?,
    );
    Ok(prepared)
}

/// Extender: crea el monitor virtual de GNOME con la resolución `size` y
/// arranca su captura.
async fn open_virtual_monitor(
    prepared: &mut Prepared,
    backend: VirtualBackend,
    cfg: &SenderConfig,
    size: (u32, u32),
    hz: u32,
) -> Result<()> {
    match backend {
        VirtualBackend::Mutter => {
            tracing::info!("creando monitor virtual de GNOME {}x{}", size.0, size.1);
            let mutter = MutterVirtual::open(cfg.accept_input).await?;
            let node = mutter.node_id;
            prepared.system = Some(SystemSession::Mutter(mutter));
            let fd = local_pipewire()?;
            prepared.capture = Some(
                start_capture(
                    FrameSource::PipeWire {
                        node,
                        fd,
                        fixed_size: Some(size),
                    },
                    cfg,
                )
                .await?,
            );
        }
        VirtualBackend::Sway => {
            tracing::info!("creando salida virtual de Sway {}x{}@{hz}", size.0, size.1);
            let output =
                tokio::task::spawn_blocking(move || SwayVirtualOutput::create(size.0, size.1, hz))
                    .await
                    .context("creando la salida de Sway")??;
            let name = output.name().to_owned();
            prepared.system = Some(SystemSession::Sway(output));
            prepared.capture = Some(
                start_capture(
                    FrameSource::Wlr {
                        output: name,
                        fps: hz,
                    },
                    cfg,
                )
                .await?,
            );
        }
    }
    Ok(())
}

/// Resolución para el monitor virtual: la pedida o, si no, la nativa del
/// receptor con `fallback` de reserva; solo modos válidos
/// (`Mode::validate`: 320..=8192 por lado), en dimensiones pares (H.264
/// 4:2:0). `None` si ninguno es válido.
fn virtual_size(requested: Option<Mode>, native: Mode, fallback: Mode) -> Option<(u32, u32)> {
    let candidates = match requested {
        Some(m) => vec![m],
        None => vec![native, fallback],
    };
    candidates
        .into_iter()
        // Pares antes de validar (validate rechaza dimensiones impares).
        .map(|m| Mode::new(m.width & !1, m.height & !1, 60))
        .find(|m| m.validate().is_ok())
        .map(|m| (m.width, m.height))
}

/// Paso 2: sesión con el receptor sobre `conn` hasta que termine. Libera
/// siempre la captura y la sesión del sistema antes de volver.
pub(crate) async fn run(
    conn: Connection,
    cfg: SenderConfig,
    prepared: Prepared,
    shutdown: watch::Receiver<bool>,
) -> Result<SenderReport> {
    let started_at = Instant::now();
    let mut prepared = prepared;
    let outcome = session(conn, &cfg, &mut prepared, shutdown).await;
    let worker_stats = prepared.release().await;
    drop(prepared);
    let (end_reason, control, send_task, conn) = outcome?;
    let send = match send_task {
        Some(t) => t.await.map(|(s, _)| s).unwrap_or_default(),
        None => Default::default(),
    };
    conn.close(b"bye");
    // Si terminó la captura, decir por qué: fallo con causa, o fin normal
    // (se dejó de compartir).
    let end_reason = match (&worker_stats.fatal, end_reason.as_str()) {
        (Some(f), "fallo en captura/codificación") => format!("{end_reason}: {f}"),
        (None, "fallo en captura/codificación") => {
            "se dejó de compartir la pantalla (el sistema cerró la captura)".to_owned()
        }
        _ => end_reason,
    };
    tracing::info!("fin de la transmisión: {end_reason}");
    Ok(SenderReport {
        duration: started_at.elapsed(),
        end_reason,
        worker: worker_stats,
        send,
        control,
    })
}

type SendTask = tokio::task::JoinHandle<(crate::stats::SendStats, tokio_mpsc::Receiver<LinuxUnit>)>;
type SessionOutcome = (String, ControlStats, Option<SendTask>, Connection);

async fn session(
    mut conn: Connection,
    cfg: &SenderConfig,
    prepared: &mut Prepared,
    mut shutdown: watch::Receiver<bool>,
) -> Result<SessionOutcome> {
    let mut ctl = FramedStream::new(conn.open_bi().await.context("abriendo canal de control")?);
    ctl.send(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        device_id: cfg.device_id,
        device_name: cfg.device_name.clone(),
        roles: Roles {
            source: true,
            sink: false,
        },
    }))
    .await?;
    let _peer = expect_hello(&mut ctl).await?;
    let sink = expect_sink(&mut ctl).await?;
    if !sink.decoders.iter().any(|d| d.codec == Codec::H264) {
        bail!("el receptor no anuncia decodificador H.264");
    }
    let stream_mode = if let Some(backend) = prepared.extend {
        // Extender: el monitor virtual toma la resolución del receptor. El
        // visor espera StartStream 10 s; crear el monitor tarda ~1 s.
        let hz = cfg
            .requested_mode
            .unwrap_or(sink.native_mode)
            .refresh_mhz
            .div_ceil(1000)
            .clamp(30, 240);
        let opened = match virtual_size(cfg.requested_mode, sink.native_mode, cfg.fallback_mode) {
            Some(size) => open_virtual_monitor(prepared, backend, cfg, size, hz).await,
            None => Err(anyhow!(
                "resolución no válida para el monitor virtual (receptor {}x{})",
                sink.native_mode.width,
                sink.native_mode.height
            )),
        };
        if let Err(e) = opened {
            let _ = ctl
                .send(&ControlMessage::Bye {
                    reason: ByeReason::Error,
                })
                .await;
            ctl.finish();
            return Err(e);
        }
        StreamMode::Extend
    } else if prepared.portal_virtual {
        StreamMode::Extend
    } else {
        StreamMode::Duplicate
    };
    let capture = prepared
        .capture
        .as_mut()
        .context("la captura no está en marcha")?;
    // Tamaño real más reciente de la captura (puede haber cambiado).
    let (width, height) = (*capture.size_rx.borrow()).context("sin tamaño de captura")?;
    let mode = Mode::new(width, height, 60);
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: STREAM_ID,
        mode: stream_mode,
        codec: Codec::H264,
        display_mode: mode,
        bitrate_kbps: cfg.bitrate_kbps,
    }))
    .await?;
    expect_started(&mut ctl).await?;
    let video_rx = capture.video_rx.take().context("fuente Linux ya usada")?;
    let (mut stop_video, stop_rx) = watch::channel(false);
    let mut send_task = tokio::spawn(send_video(
        conn.clone(),
        video_rx,
        cfg.clone(),
        Duration::from_micros(16_667),
        Default::default(),
        stop_rx,
    ));
    let worker = &capture.worker;
    worker.shared.force_idr.store(true, Ordering::Relaxed);
    worker.shared.streaming.store(true, Ordering::Relaxed);
    tracing::info!("transmisión iniciada ({stream_mode:?}): {width}x{height}");

    let mut cs = ControlStats::default();
    let mut last_forced: Option<Instant> = None;
    let deadline = cfg.duration.map(|d| tokio::time::Instant::now() + d);
    let mut health = tokio::time::interval(Duration::from_secs(1));
    // Señal del sistema: el usuario dejó de compartir desde GNOME.
    let system = prepared.system.as_ref();
    let can_inject = cfg.accept_input && system.is_some_and(SystemSession::can_inject);
    tracing::info!(
        "entrada remota (ratón y teclado): {}",
        if can_inject { "sí" } else { "no" }
    );
    let mut pressed = PressedState::new();
    let mut wheel = evdev::WheelAccumulator::default();
    // Portapapeles de texto (opcional); si no se puede abrir, se sigue sin él.
    let mut clipboard = if cfg.clipboard {
        match cs_clipboard::ClipboardBridge::spawn(cs_clipboard::bridge::DEFAULT_POLL) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("portapapeles no disponible: {e}");
                None
            }
        }
    } else {
        None
    };
    let system_closed = async {
        match system {
            Some(s) => s.wait_closed().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(system_closed);
    let mut peer_gone = false;
    // Audio del sistema (ADR-0008): solo si está activado y el receptor lo pide.
    let mut audio: Option<crate::audio::AudioSender> = None;
    // Archivos (ADR-0010): solo si están activados.
    let mut files = crate::files::start(&conn, cfg, &mut ctl).await;
    let end_reason: String = 'session: loop {
        let (drop_reason, connection_lost) = loop {
            tokio::select! {
                _ = health.tick() => {}
                Some(msg) = cs_clipboard::bridge::next_outgoing(&mut clipboard) => {
                    if let Err(e) = ctl.send(&ControlMessage::ClipboardText(msg)).await {
                        break (format!("conexión de control perdida: {e}"), true);
                    }
                    cs.clipboard_sent += 1;
                }
                _ = &mut system_closed => {
                    break ("se dejó de compartir la pantalla desde el sistema".to_owned(), false);
                }
                result = shutdown.changed() => {
                    break if result.is_err() || *shutdown.borrow() {
                        ("parada pedida".to_owned(), false)
                    } else {
                        ("canal de parada cerrado".to_owned(), false)
                    };
                }
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => break ("duración cumplida (--seconds)".to_owned(), false),
                m = ctl.recv::<ControlMessage>() => match m {
                    Ok(Some(ControlMessage::RequestKeyframe { .. })) => {
                        cs.keyframe_requests += 1;
                        let now = Instant::now();
                        if last_forced.is_none_or(|t| now.duration_since(t) >= MIN_IDR_INTERVAL) {
                            last_forced = Some(now);
                            worker.shared.force_idr.store(true, Ordering::Relaxed);
                            cs.keyframes_forced += 1;
                        } else {
                            cs.keyframe_requests_ignored += 1;
                        }
                    }
                    Ok(Some(ControlMessage::ReceiverStats(s))) => {
                        cs.receiver_stats += 1;
                        tracing::debug!(?s, "ReceiverStats");
                        cs.last_receiver_stats = Some(s);
                    }
                    Ok(Some(ControlMessage::Bye { reason })) => {
                        peer_gone = true;
                        break (format!("el receptor se despidió ({reason:?})"), false);
                    }
                    Ok(Some(ControlMessage::StopStream { .. })) => {
                        break ("el receptor pidió StopStream".to_owned(), false);
                    }
                    Ok(Some(ControlMessage::Input(ev))) => {
                        if can_inject && ev.stream_id() == STREAM_ID {
                            let size = (*capture.size_rx.borrow()).unwrap_or((width, height));
                            pressed.record(&ev);
                            for action in to_actions(&ev, size, &mut wheel) {
                                match system.map(|s| s.inject(&action)) {
                                    Some(f) => match f.await {
                                        Ok(()) => cs.input_events_injected += 1,
                                        Err(e) => {
                                            cs.input_events_blocked += 1;
                                            tracing::debug!("entrada remota no inyectada: {e}");
                                        }
                                    },
                                    None => cs.input_events_blocked += 1,
                                }
                            }
                        }
                    }
                    Ok(Some(ControlMessage::ClipboardText(t))) => {
                        if let Some(b) = clipboard.as_ref() {
                            cs.clipboard_received += 1;
                            b.deliver(t);
                        }
                    }
                    Ok(Some(ControlMessage::FilesAvailable { enabled })) => {
                        crate::files::on_peer(&files, enabled);
                    }
                    Ok(Some(ControlMessage::AudioRequest { enabled })) => {
                        if enabled && cfg.audio && audio.is_none() {
                            if ctl
                                .send(&ControlMessage::AudioStart(crate::audio::audio_start()))
                                .await
                                .is_ok()
                            {
                                match crate::audio::AudioSender::start(conn.clone()) {
                                    Ok(a) => audio = Some(a),
                                    Err(e) => {
                                        tracing::warn!("audio no disponible: {e:#}");
                                        let _ = ctl
                                            .send(&ControlMessage::AudioStop {
                                                stream_id: crate::audio::AUDIO_STREAM_ID,
                                            })
                                            .await;
                                    }
                                }
                            }
                        } else if !enabled {
                            if let Some(a) = audio.take() {
                                cs.audio_frames_sent += a.sent();
                                let _ = ctl
                                    .send(&ControlMessage::AudioStop {
                                        stream_id: crate::audio::AUDIO_STREAM_ID,
                                    })
                                    .await;
                            }
                        }
                    }
                    // Ping y demás: no se usan en Linux.
                    Ok(Some(_)) => {}
                    Ok(None) => break ("el receptor cerró el canal de control".to_owned(), true),
                    Err(error) => break (format!("conexión de control perdida: {error}"), true),
                },
            }
            if worker.finished() {
                break ("fallo en captura/codificación".to_owned(), false);
            }
        };
        // Nada debe quedar pulsado en GNOME (teclas, botones) tras un corte o al
        // terminar.
        if let Some(sys) = system.filter(|_| can_inject) {
            let size = (*capture.size_rx.borrow()).unwrap_or((width, height));
            for ev in pressed.release_all() {
                for action in to_actions(&ev, size, &mut wheel) {
                    let _ = sys.inject(&action).await;
                }
            }
        }
        // Corte de red: reconectar sin soltar la captura ni el monitor virtual.
        let Some(reconn) = cfg
            .reconnect
            .as_ref()
            .filter(|r| connection_lost && !r.grace.is_zero())
        else {
            break 'session drop_reason;
        };
        peer_gone = true;
        tracing::warn!(
            reason = %drop_reason,
            grace_s = reconn.grace.as_secs_f64(),
            "conexión interrumpida; intentando reconectar sin soltar la captura"
        );
        tracing::info!(
            "Conexión interrumpida ({drop_reason}); intentando reconectar durante {:.0} s…",
            reconn.grace.as_secs_f64()
        );
        worker.shared.streaming.store(false, Ordering::Relaxed);
        // El audio iba por la conexión caída: el receptor lo pedirá de nuevo.
        if let Some(a) = audio.take() {
            cs.audio_frames_sent += a.sent();
        }
        let _ = stop_video.send(true);
        let (sstats, mut recovered_rx) =
            match tokio::time::timeout(Duration::from_secs(2), &mut send_task).await {
                Ok(Ok(pair)) => pair,
                _ => break 'session drop_reason,
            };
        conn.close(b"reconnecting");
        let size = (*capture.size_rx.borrow()).unwrap_or((width, height));
        let display_mode = Mode::new(size.0, size.1, 60);
        match crate::reconnect::try_reconnect(
            reconn,
            cfg,
            display_mode,
            stream_mode,
            &mut shutdown,
            deadline,
        )
        .await
        {
            Ok(Some((new_conn, new_ctl))) => {
                conn = new_conn;
                ctl = new_ctl;
                files = crate::files::start(&conn, cfg, &mut ctl).await;
                peer_gone = false;
                while recovered_rx.try_recv().is_ok() {}
                worker.shared.force_idr.store(true, Ordering::Relaxed);
                worker.shared.streaming.store(true, Ordering::Relaxed);
                let (tx, rx) = watch::channel(false);
                stop_video = tx;
                send_task = tokio::spawn(send_video(
                    conn.clone(),
                    recovered_rx,
                    cfg.clone(),
                    Duration::from_micros(16_667),
                    sstats,
                    rx,
                ));
                cs.reconnections += 1;
                tracing::info!(
                    "Reconectado con {} (reconexión #{})",
                    conn.remote_addr(),
                    cs.reconnections
                );
            }
            Ok(None) => {
                send_task = tokio::spawn(async move { (sstats, recovered_rx) });
                break 'session if *shutdown.borrow() {
                    "parada pedida".to_owned()
                } else {
                    "duración cumplida (--seconds)".to_owned()
                };
            }
            Err(e) => {
                tracing::warn!("no se pudo reconectar en la ventana de gracia: {e:#}");
                send_task = tokio::spawn(async move { (sstats, recovered_rx) });
                break 'session format!("{drop_reason} (reconexión agotada: {e})");
            }
        }
    };
    if let Some(a) = audio.take() {
        cs.audio_frames_sent += a.sent();
    }
    if !peer_gone {
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            ctl.send(&ControlMessage::Bye {
                reason: ByeReason::UserRequested,
            }),
        )
        .await;
        ctl.finish();
    }
    let _ = stop_video.send(true);
    Ok((end_reason, cs, Some(send_task), conn))
}

async fn expect_hello(ctl: &mut FramedStream) -> Result<cs_protocol::control::Hello> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, ctl.expect::<ControlMessage>()).await?? {
        ControlMessage::Hello(hello) => Ok(hello),
        ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
        _ => bail!("se esperaba Hello del receptor"),
    }
}

async fn expect_sink(ctl: &mut FramedStream) -> Result<SinkCapabilities> {
    loop {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, ctl.expect::<ControlMessage>()).await?? {
            ControlMessage::SinkCapabilities(c) => return Ok(c),
            ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
            _ => {}
        }
    }
}

async fn expect_started(ctl: &mut FramedStream) -> Result<()> {
    loop {
        match tokio::time::timeout(START_TIMEOUT, ctl.expect::<ControlMessage>()).await?? {
            ControlMessage::StreamStarted { stream_id } if stream_id == STREAM_ID => return Ok(()),
            ControlMessage::StreamFailed { reason, .. } => {
                bail!("el receptor no pudo iniciar el flujo: {reason}")
            }
            ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
            _ => {}
        }
    }
}

/// Envía las unidades codificadas por datagramas hasta que se pida parar o
/// se cierre la conexión. Devuelve la cola de vídeo para reutilizarla tras
/// una reconexión.
async fn send_video(
    conn: Connection,
    mut rx: tokio_mpsc::Receiver<LinuxUnit>,
    cfg: SenderConfig,
    interval: Duration,
    mut stats: crate::stats::SendStats,
    mut stop: watch::Receiver<bool>,
) -> (crate::stats::SendStats, tokio_mpsc::Receiver<LinuxUnit>) {
    let Some(max_datagram) = conn.max_datagram_size() else {
        tracing::error!("la conexión no admite datagramas: no se puede enviar vídeo");
        return (stats, rx);
    };
    let packetizer = Packetizer::new(
        STREAM_ID,
        PacketizerConfig {
            max_datagram,
            fec_ratio: cfg.fec_ratio,
        },
    )
    .with_keyframe_fec_ratio(cfg.keyframe_fec_ratio)
    .with_interleave(cfg.fec_interleave);
    let mut pacer = Pacer::new(PacerConfig {
        mode: cfg.pacing,
        burst_bytes: max_datagram * 4,
    });
    let opts = PacedSendOptions {
        wait: cfg.pacing_wait,
        ..PacedSendOptions::default()
    };
    let mut sequence = 0u32;
    loop {
        let unit = tokio::select! {
            _ = stop.changed() => break,
            _ = conn.closed() => break,
            unit = rx.recv() => match unit {
                Some(u) => u,
                None => break,
            },
        };
        let meta = FrameMeta {
            frame_seq: sequence,
            pts_us: wire_pts(unit.pts_us),
            keyframe: unit.keyframe,
            discardable: false,
        };
        sequence = sequence.wrapping_add(1);
        let datagrams = match packetizer.packetize(meta, &unit.data) {
            Ok(datagrams) => datagrams,
            Err(error) => {
                tracing::warn!("no se pudo fragmentar un frame Linux: {error}");
                continue;
            }
        };
        let sizes = datagrams.iter().map(Vec::len).collect::<Vec<_>>();
        let schedule = pacer.schedule(&sizes, interval, Instant::now());
        let connection = conn.clone();
        let result = pacer::send_paced(
            datagrams.into_iter().map(Bytes::from).collect(),
            schedule,
            opts,
            move |datagram| connection.send_datagram(datagram).is_ok(),
        )
        .await;
        stats.frames_sent += 1;
        stats.keyframes_sent += u64::from(unit.keyframe);
        stats.payload_bytes += unit.data.len() as u64;
        stats.datagrams_sent += result.sent as u64;
        stats.datagrams_failed += result.failed as u64;
        stats.max_pacing_late = stats.max_pacing_late.max(result.max_late);
    }
    (stats, rx)
}

#[cfg(test)]
mod tests {
    use super::{to_actions, virtual_size};
    use cs_capture::linux_input::InputAction;
    use cs_core::Mode;
    use cs_input::{evdev::WheelAccumulator, ButtonState, InputEvent, MouseButton};

    #[test]
    fn protocol_events_become_gnome_actions() {
        let mut w = WheelAccumulator::default();
        let size = (1920, 1080);
        assert_eq!(
            to_actions(
                &InputEvent::MouseMove {
                    stream_id: 1,
                    x_norm: u16::MAX,
                    y_norm: 0
                },
                size,
                &mut w
            ),
            vec![InputAction::PointerAbs { x: 1919.0, y: 0.0 }]
        );
        assert_eq!(
            to_actions(
                &InputEvent::MouseButton {
                    stream_id: 1,
                    button: MouseButton::Right,
                    state: ButtonState::Pressed
                },
                size,
                &mut w
            ),
            vec![InputAction::Button {
                code: 0x111,
                pressed: true
            }]
        );
        // Rueda: un escalón arriba en Windows = -1 en Linux.
        assert_eq!(
            to_actions(
                &InputEvent::MouseWheel {
                    stream_id: 1,
                    delta_x: 0,
                    delta_y: 120
                },
                size,
                &mut w
            ),
            vec![InputAction::Axis {
                horizontal: false,
                steps: -1
            }]
        );
        // Flecha arriba (E0 48) y tecla sin equivalente.
        let key = |scancode, extended| InputEvent::Key {
            stream_id: 1,
            scancode,
            extended,
            state: ButtonState::Released,
        };
        assert_eq!(
            to_actions(&key(0x48, true), size, &mut w),
            vec![InputAction::Key {
                code: 103,
                pressed: false
            }]
        );
        assert!(to_actions(&key(0x7E, true), size, &mut w).is_empty());
    }

    #[test]
    fn virtual_monitor_uses_valid_receiver_mode_with_even_size() {
        let fb = Mode::new(1920, 1080, 60);
        let native = Mode::new(1366, 768, 60);
        assert_eq!(virtual_size(None, native, fb), Some((1366, 768)));
        assert_eq!(
            virtual_size(None, Mode::new(1365, 767, 60), fb),
            Some((1364, 766))
        );
        assert_eq!(
            virtual_size(Some(Mode::new(2560, 1440, 60)), native, fb),
            Some((2560, 1440))
        );
        // Receptor con un modo absurdo: se usa la reserva.
        assert_eq!(
            virtual_size(None, Mode::new(100_000, 100_000, 60), fb),
            Some((1920, 1080))
        );
        assert_eq!(
            virtual_size(None, Mode::new(0, 0, 60), fb),
            Some((1920, 1080))
        );
        // Modo pedido inválido: no hay monitor.
        assert_eq!(virtual_size(Some(Mode::new(10, 10, 60)), native, fb), None);
    }
}
