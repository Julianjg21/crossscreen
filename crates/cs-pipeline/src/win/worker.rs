//! Hilo de captura + codificación (Windows).
//!
//! Un único hilo con prioridad alta (MMCSS "Games") hace, por frame:
//! `DxgiCapture::next_frame` → copia a textura propia (en `cs-capture`) →
//! `MfH264Encoder::encode` sobre **el mismo** `ID3D11Device` → entrega la
//! unidad Annex-B a la tarea de envío por un canal acotado de capacidad 1.
//!
//! Reglas de latencia:
//! - Si el canal está lleno (la red no da abasto), el frame se descarta
//!   **antes** de codificarlo: no se acumula cola y la cadena de referencias
//!   del H.264 sigue intacta (el siguiente P referencia al último enviado).
//! - Escritorio estático: no se envía nada, salvo (a) un IDR pedido por el
//!   receptor, que se atiende recodificando la última imagen, y (b) un
//!   refresco opcional cada `static_refresh` (P casi vacío) para que el
//!   receptor detecte la pérdida del último frame y pueda pedir un IDR.

use super::priority::ThreadPriorityGuard;
use crate::cursor::{hotspot_position, shape_to_message};
use crate::stats::WorkerStats;
use crate::timing::PtsClock;
use cs_capture::windows_dxgi::{poll_cursor_in_rect, qpc_now_us, D3D11Texture, DxgiCapture};
use cs_capture::{CaptureConfig, CaptureError, CaptureSource, CaptureTarget, Rect};
use cs_encode::mf::{InputFrame, MfH264Encoder};
use cs_encode::{EncodeControl, EncoderConfig, VideoEncoder};
use cs_protocol::control::{CursorShape, StreamId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// Unidad codificada lista para enviar.
#[derive(Debug)]
pub struct VideoUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Instante de captura, µs de QPC completos.
    pub pts_us: u64,
}

/// Posición del cursor (el *hotspot*, en píxeles del monitor capturado).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// Parámetros del hilo.
#[derive(Debug, Clone)]
pub struct WorkerParams {
    pub target: CaptureTarget,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub stream_id: StreamId,
    pub static_refresh: Option<Duration>,
    pub intra_refresh: Option<u32>,
    /// Códec acordado con el receptor (ADR-0012).
    pub codec: cs_core::Codec,
}

/// Órdenes del lado async al hilo.
#[derive(Debug, Default)]
pub struct WorkerControl {
    pub stop: AtomicBool,
    /// El receptor confirmó `StreamStarted`: a partir de aquí se codifica.
    pub streaming: AtomicBool,
    pub force_idr: AtomicBool,
}

/// Salidas del hilo.
#[derive(Debug)]
pub struct WorkerOutputs {
    pub video: mpsc::Sender<VideoUnit>,
    pub cursor_pos: watch::Sender<Option<CursorPos>>,
    pub cursor_shape: watch::Sender<Option<Arc<CursorShape>>>,
    pub desktop_rect: watch::Sender<Option<Rect>>,
}

/// Resultado del arranque: tamaño de la salida capturada.
pub type Ready = Result<(u32, u32), String>;

/// Lanza el hilo. `ready` se resuelve tras abrir la captura y el codificador;
/// `done` cuando el hilo termina (por parada o por error fatal).
pub fn spawn(
    params: WorkerParams,
    out: WorkerOutputs,
    ctl: Arc<WorkerControl>,
) -> std::io::Result<(
    std::thread::JoinHandle<WorkerStats>,
    oneshot::Receiver<Ready>,
    oneshot::Receiver<()>,
)> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();
    let h = std::thread::Builder::new()
        .name("cs-capture-encode".into())
        .spawn(move || {
            let st = Worker::run(params, out, ctl, ready_tx);
            let _ = done_tx.send(());
            st
        })?;
    Ok((h, ready_rx, done_rx))
}

struct Encoder {
    inner: MfH264Encoder,
    size: (u32, u32),
    device_generation: u32,
}

struct Worker {
    params: WorkerParams,
    out: WorkerOutputs,
    ctl: Arc<WorkerControl>,
    st: WorkerStats,
    enc: Option<Encoder>,
    need_idr: bool,
    consecutive_errors: u32,
    last_encode_at: Instant,
    shape_gen: u64,
    pos_gen: u64,
    last_shape: Option<Arc<CursorShape>>,
}

/// Errores seguidos del codificador antes de recrearlo, y antes de rendirse.
const ERRORS_BEFORE_RECREATE: u32 = 3;
const ERRORS_FATAL: u32 = 9;

fn open_capture(target: &CaptureTarget, stop: &AtomicBool) -> Result<DxgiCapture, CaptureError> {
    let cfg = CaptureConfig {
        collect_metadata: false,
        ..CaptureConfig::default()
    };
    // Justo después de `display show` la salida puede no estar aún en DXGI
    // (topología asentándose, PoC-2 hallazgo 2): reintentar unos segundos.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match DxgiCapture::new(target.clone(), cfg.clone()) {
            Ok(c) => return Ok(c),
            Err(e @ (CaptureError::OutputNotFound(_) | CaptureError::Os { .. }))
                if Instant::now() < deadline && !stop.load(Ordering::Relaxed) =>
            {
                tracing::debug!("captura aún no disponible ({e}); reintentando");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

impl Worker {
    fn run(
        params: WorkerParams,
        out: WorkerOutputs,
        ctl: Arc<WorkerControl>,
        ready: oneshot::Sender<Ready>,
    ) -> WorkerStats {
        let prio = ThreadPriorityGuard::raise();
        tracing::info!(priority = ?prio.priority, "hilo de captura+codificación");
        let mut w = Worker {
            params,
            out,
            ctl,
            st: WorkerStats::default(),
            enc: None,
            need_idr: true,
            consecutive_errors: 0,
            last_encode_at: Instant::now(),
            shape_gen: 0,
            pos_gen: 0,
            last_shape: None,
        };
        let mut cap = match open_capture(&w.params.target, &w.ctl.stop) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("no se pudo abrir la captura de {}: {e}", w.params.target);
                let _ = ready.send(Err(msg.clone()));
                w.st.fatal = Some(msg);
                return w.st;
            }
        };
        let size = cap
            .duplication_info()
            .map(|d| (d.width, d.height))
            .unwrap_or((0, 0));
        tracing::info!(output = ?cap.output(), info = ?cap.duplication_info(), "captura abierta");
        let _ = w.out.desktop_rect.send(Some(cap.output().desktop_rect));
        // Crear el codificador ya, para que un fallo se informe antes de
        // empezar la sesión y no con el primer frame.
        if size.0 > 0 {
            if let Err(e) = w.ensure_encoder(&cap, size, 0) {
                let msg = format!("no se pudo crear el codificador H.264: {e}");
                let _ = ready.send(Err(msg.clone()));
                w.st.fatal = Some(msg);
                return w.st;
            }
        }
        let _ = ready.send(Ok(size));

        let interval = Duration::from_micros(1_000_000 / u64::from(w.params.fps.max(1)));
        let mut clock = PtsClock::default();
        let mut last_tex: Option<(D3D11Texture, (u32, u32), u32)> = None;
        while !w.ctl.stop.load(Ordering::Relaxed) {
            let r = cap.next_frame(interval);
            w.publish_cursor(&cap);
            let streaming = w.ctl.streaming.load(Ordering::Relaxed);
            let res = match r {
                Err(e) => Err(format!("captura: {e}")),
                Ok(Some(f)) => {
                    w.st.captured += 1;
                    w.st.last_size = Some((f.width, f.height));
                    w.st.capture_us.push(f.timing.capture_us() as f64);
                    if let Some(d) = f.timing.present_to_acquire_us() {
                        w.st.present_to_acquire_us.push(d as f64);
                    }
                    if f.accumulated_frames > 1 {
                        w.st.accumulated_gt1 += 1;
                    }
                    let pts = clock.next(f.timing.present_us, f.timing.acquired_us);
                    let size = (f.width, f.height);
                    last_tex = Some((f.texture.clone(), size, f.device_generation));
                    if streaming {
                        w.encode_and_send(&cap, &f.texture, size, f.device_generation, pts)
                    } else {
                        Ok(())
                    }
                }
                Ok(None) => match &last_tex {
                    Some((tex, size, generation)) if streaming => {
                        let idr = w.ctl.force_idr.load(Ordering::Relaxed) || w.need_idr;
                        let channel_idle = w.out.video.capacity() >= w.out.video.max_capacity();
                        let refresh = channel_idle
                            && w.params
                                .static_refresh
                                .is_some_and(|r| w.last_encode_at.elapsed() >= r);
                        if idr || refresh {
                            w.st.repeats += 1;
                            let pts = clock.next(0, qpc_now_us());
                            let (tex, size, generation) = (tex.clone(), *size, *generation);
                            w.encode_and_send(&cap, &tex, size, generation, pts)
                        } else {
                            Ok(())
                        }
                    }
                    _ => Ok(()),
                },
            };
            if let Err(e) = res {
                tracing::error!("{e}");
                w.st.fatal = Some(e);
                break;
            }
        }
        let cs = cap.stats();
        w.st.access_lost = cs.access_lost;
        w.st.capture_reinits = cs.reinits;
        // El codificador usa el dispositivo de la captura: cerrarlo antes.
        w.enc = None;
        drop(last_tex);
        drop(cap);
        drop(prio);
        w.st
    }

    fn ensure_encoder(
        &mut self,
        cap: &DxgiCapture,
        size: (u32, u32),
        device_generation: u32,
    ) -> Result<(), String> {
        if self
            .enc
            .as_ref()
            .is_some_and(|e| e.size == size && e.device_generation == device_generation)
        {
            return Ok(());
        }
        if let Some(old) = self.enc.take() {
            tracing::info!(
                old = ?old.size,
                new = ?size,
                "recreando el codificador (cambio de tamaño, de dispositivo o tras errores)"
            );
        }
        let mut cfg =
            EncoderConfig::desktop(size.0, size.1, self.params.fps, self.params.bitrate_kbps);
        cfg.intra_refresh = self.params.intra_refresh;
        cfg.codec = self.params.codec;
        let inner = MfH264Encoder::new(cap.device(), cfg).map_err(|e| e.to_string())?;
        tracing::info!(
            encoder = %inner.caps().info.friendly_name,
            nv12_bind = inner.nv12_bind_flags(),
            codec = ?self.params.codec,
            "codificador listo"
        );
        self.st.encoders_created += 1;
        self.enc = Some(Encoder {
            inner,
            size,
            device_generation,
        });
        self.need_idr = true;
        Ok(())
    }

    /// Codifica `tex` y entrega el resultado. `Err` solo si el fallo es fatal.
    fn encode_and_send(
        &mut self,
        cap: &DxgiCapture,
        tex: &D3D11Texture,
        size: (u32, u32),
        device_generation: u32,
        pts_us: u64,
    ) -> Result<(), String> {
        // Contrapresión: para P-frames normales exigimos que el canal (cap. 2)
        // esté vacío (`capacity() >= 2`), de modo que no se acumule cola; un
        // IDR forzado puede usar la plaza de reserva (`capacity() >= 1`) para
        // no retrasar la recuperación ante pérdidas.
        let want_idr = self.need_idr || self.ctl.force_idr.load(Ordering::Relaxed);
        let min_cap = if want_idr {
            1
        } else {
            self.out.video.max_capacity()
        };
        if self.out.video.capacity() < min_cap {
            self.st.dropped_backpressure += 1;
            return Ok(());
        }
        if self.out.video.is_closed() {
            return Ok(());
        }
        if let Err(e) = self.ensure_encoder(cap, size, device_generation) {
            return self.on_encoder_error(format!("crear codificador: {e}"));
        }
        let force = self.need_idr | self.ctl.force_idr.swap(false, Ordering::Relaxed);
        if force {
            self.st.forced_idr += 1;
        }
        let Some(enc) = self.enc.as_mut() else {
            return Ok(());
        };
        let input = InputFrame {
            texture: tex.texture().clone(),
            subresource: 0,
            pts_us,
        };
        let ctl = EncodeControl {
            force_idr: force,
            bitrate_kbps: None,
        };
        match enc.inner.encode(&input, ctl) {
            Ok(units) => {
                self.consecutive_errors = 0;
                self.last_encode_at = Instant::now();
                let emitted_idr = units.iter().any(|u| u.keyframe);
                if force && emitted_idr {
                    self.need_idr = false;
                } else if force {
                    // El MFT aún no entregó un IDR: mantener la petición para el siguiente frame.
                    self.ctl.force_idr.store(true, Ordering::Relaxed);
                    self.need_idr = false;
                }
                for u in units {
                    self.st.encoded += 1;
                    self.st.encode_us.push(u.encode_time.as_micros() as f64);
                    self.st.hw_us.push(u.hw_time.as_micros() as f64);
                    if u.keyframe {
                        self.st.idr_out += 1;
                        self.st.idr_bytes += u.data.len() as u64;
                    } else {
                        self.st.p_bytes += u.data.len() as u64;
                    }
                    let unit = VideoUnit {
                        data: u.data,
                        keyframe: u.keyframe,
                        pts_us: u.pts_us,
                    };
                    if self.out.video.try_send(unit).is_err() {
                        // Una unidad codificada perdida rompe la cadena: IDR.
                        self.st.dropped_after_encode += 1;
                        self.need_idr = true;
                    }
                }
                Ok(())
            }
            Err(e) => {
                if force {
                    self.need_idr = true;
                }
                self.on_encoder_error(format!("codificar: {e}"))
            }
        }
    }

    fn on_encoder_error(&mut self, msg: String) -> Result<(), String> {
        self.st.encoder_errors += 1;
        self.consecutive_errors += 1;
        tracing::warn!(
            seguidos = self.consecutive_errors,
            "error del codificador: {msg}"
        );
        if self.consecutive_errors >= ERRORS_FATAL {
            return Err(format!(
                "el codificador falló {} veces seguidas; último error: {msg}",
                self.consecutive_errors
            ));
        }
        if self.consecutive_errors % ERRORS_BEFORE_RECREATE == 0 {
            self.enc = None; // se recrea en el siguiente frame
        }
        Ok(())
    }

    fn publish_cursor(&mut self, cap: &DxgiCapture) {
        let rect = cap.output().desktop_rect;
        self.out.desktop_rect.send_if_modified(|old| {
            let changed = *old != Some(rect);
            *old = Some(rect);
            changed
        });
        let polled = poll_cursor_in_rect(rect);
        if let Some((x, y, visible)) = polled {
            let p = CursorPos { x, y, visible };
            self.out.cursor_pos.send_if_modified(|old| {
                let changed = *old != Some(p);
                *old = Some(p);
                changed
            });
        }
        let cur = cap.cursor();
        let shape_changed = cur.shape_generation != self.shape_gen;
        if shape_changed {
            self.shape_gen = cur.shape_generation;
            if let Some(m) = cur
                .shape
                .as_ref()
                .and_then(|s| shape_to_message(s, self.params.stream_id))
            {
                let identical = self.last_shape.as_deref() == Some(&m);
                if !identical {
                    let arc = Arc::new(m);
                    self.last_shape = Some(arc.clone());
                    self.out.cursor_shape.send_replace(Some(arc));
                }
            }
        }
        if cur.position_generation != self.pos_gen || shape_changed {
            self.pos_gen = cur.position_generation;
            if polled.is_none() {
                if let Some(tl) = cur.position {
                    let (x, y) = hotspot_position(tl, cur.shape.as_ref());
                    let p = CursorPos {
                        x,
                        y,
                        visible: cur.visible,
                    };
                    self.out.cursor_pos.send_if_modified(|old| {
                        let changed = *old != Some(p);
                        *old = Some(p);
                        changed
                    });
                }
            }
        }
    }
}
