//! Tipos compartidos entre los tres hilos del receptor:
//!
//! - **red** (tokio): QUIC, canal de control, reensamblado;
//! - **decodificación** (hilo propio, mismo dispositivo D3D11);
//! - **ventana** (winit): presentación con el objeto *waitable* y cursor.

use cs_core::Mode;
use cs_decode::stats::LatencyStats;
use cs_decode::DecodedFrame;
use cs_transport::CompleteFrame;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Eventos que despiertan al hilo de la ventana (vía `EventLoopProxy`).
#[derive(Debug)]
pub enum UserEvent {
    /// Empieza una sesión: crear ventana, renderer y decodificador para
    /// `display_mode` y responder por el `oneshot` cuando estén listos.
    Start(
        Box<StartSession>,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
    /// Forma nueva del cursor (ya validada).
    CursorShape(cs_protocol::control::CursorShape),
    /// Posición del cursor en píxeles del monitor transmitido.
    CursorPosition { x: i32, y: i32, visible: bool },
    /// Hay un frame decodificado en el buzón.
    FrameReady,
    /// La sesión terminó (la red ya soltó su extremo del canal de decodificación).
    End { reason: String },
    /// El hilo de red terminó (`--once`, cierre pedido o error fatal).
    NetDone(Option<String>),
}

/// Lo necesario para arrancar una sesión en el hilo de la ventana.
#[derive(Debug)]
pub struct StartSession {
    pub display_mode: Mode,
    pub peer: String,
    pub decode_rx: Receiver<DecodeInput>,
    pub keyframe_tx: tokio::sync::mpsc::UnboundedSender<KeyframeSignal>,
    pub stats: Arc<Mutex<SessionStats>>,
}

/// Del decodificador a la red, para `RequestKeyframe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeSignal {
    /// Hace falta un IDR: se perdió (o no se pudo decodificar) `lost_seq`,
    /// o el primer frame no era IDR.
    Needed { lost_seq: u32 },
    /// Se decodificó el IDR `frame_seq` (se avisa de **todos**, pedidos o
    /// periódicos): cura cualquier pérdida anterior a él.
    Decoded { frame_seq: u32 },
}

/// Qué hace el decodificador tras una pérdida.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LossPolicy {
    /// Por defecto: seguir entregando los P-frames al MFT, que oculta las
    /// referencias que faltan (artefactos localizados en vez de congelación),
    /// y pedir **un** IDR para sanear. Si el MFT devuelve error, `flush` y
    /// esperar al IDR (como `FreezeOnLoss`).
    #[default]
    DecodeThrough,
    /// Comportamiento anterior (`--freeze-on-loss`): descartar todos los
    /// P-frames hasta el siguiente IDR y mantener el último frame bueno.
    FreezeOnLoss,
}

/// Política de envío de `RequestKeyframe` (ver `net::KeyframeRequester`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum KeyframePolicy {
    /// Por defecto: una petición por episodio; no se repite hasta decodificar
    /// un IDR posterior a la petición o hasta `max(3·RTT, 250 ms)`, con
    /// *backoff* exponencial hasta 2 s si los IDR siguen sin llegar.
    #[default]
    Calm,
    /// Anterior: cada pérdida o P-frame descartado pide IDR; como mucho una
    /// petición cada 100 ms.
    Legacy,
}

/// Órdenes del hilo de la ventana al de red.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCmd {
    /// Cerrar la sesión en curso (si la hay) con `Bye` y dejar de escuchar.
    Shutdown,
    /// Evento de entrada local (ratón o teclado) para enviar a la fuente.
    Input(cs_protocol::control::InputEvent),
}

/// Entrada del hilo de decodificación (en orden de llegada).
#[derive(Debug)]
pub enum DecodeInput {
    Frame {
        frame: CompleteFrame,
        received_at: Instant,
    },
    /// El reensamblador declaró perdido un frame (o la cola se desbordó).
    Lost { frame_seq: u32, at: Instant },
}

/// Frame decodificado a la espera de presentarse.
#[derive(Debug)]
pub struct MailFrame {
    pub frame: DecodedFrame,
    pub frame_seq: u32,
    pub pts_us: u32,
    pub received_at: Instant,
    pub decoded_at: Instant,
    /// Decodificado con referencias rotas (entre una pérdida y el IDR que la
    /// cura): la imagen puede llevar artefactos de ocultación.
    pub concealed: bool,
}

/// Buzón de una plaza, «el último gana»: el decodificador deja el frame más
/// reciente y, si había otro sin presentar, lo descarta (nunca se encola).
#[derive(Debug, Default)]
pub struct Mailbox {
    slot: Mutex<Option<MailFrame>>,
}

impl Mailbox {
    /// Deja `f`; devuelve `true` si sustituyó a un frame no presentado.
    pub fn put(&self, f: MailFrame) -> bool {
        let old = self
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(f);
        // `old` se suelta fuera del candado: devuelve su superficie al MFT.
        old.is_some()
    }

    pub fn take(&self) -> Option<MailFrame> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Hueco entre dos frames presentados a partir del cual se cuenta una
/// congelación visible.
pub const FREEZE_THRESHOLD: Duration = Duration::from_millis(200);

/// Métricas de una sesión (las escriben los tres hilos).
#[derive(Debug, Default)]
pub struct SessionStats {
    pub started: Option<Instant>,
    // Red.
    pub datagrams: u64,
    pub frames_received: u64,
    /// Perdidos en total: los que declara el reensamblador (que ya incluye
    /// los huecos de `frame_seq`) + `frames_gap_unreported`.
    pub frames_lost: u64,
    /// De los perdidos, cuántos no tuvieron ningún datagrama recibido.
    pub frames_missing: u64,
    /// Huecos de `frame_seq` que el reensamblador no notificó (respaldo de
    /// `net::SeqTracker`; con el `cs-transport` actual debería ser 0).
    pub frames_gap_unreported: u64,
    pub shards_recovered: u64,
    pub late_datagrams: u64,
    pub decode_queue_overflows: u64,
    pub keyframe_requests_sent: u64,
    /// Peticiones repetidas porque venció la espera sin IDR (política calmada).
    pub keyframe_request_timeouts: u64,
    /// Avisos de pérdida que no generaron petición porque ya había una pendiente.
    pub keyframe_needs_suppressed: u64,
    /// RTT de QUIC (último valor leído) para la espera de las peticiones.
    pub rtt: Duration,
    pub cursor_shapes: u64,
    pub cursor_positions: u64,
    pub input_events_sent: u64,
    // Decodificación.
    pub frames_decoded: u64,
    pub decode_errors: u64,
    pub dropped_waiting_idr: u64,
    /// Veces que el MFT dejó de producir salida (tratadas como error).
    pub decode_stalls: u64,
    pub mailbox_replaced: u64,
    pub decode: LatencyStats,
    /// Política tras pérdida (para el informe).
    pub loss_policy: LossPolicy,
    /// Frames decodificados con referencias rotas (con `DecodeThrough`).
    pub frames_decoded_concealed: u64,
    /// Desde la primera pérdida de un episodio hasta el primer IDR
    /// decodificado (primera imagen totalmente limpia).
    pub recovery: LatencyStats,
    /// Por episodio cerrado: `RequestKeyframe` enviados durante el episodio.
    pub episode_requests: Vec<u64>,
    /// Por episodio cerrado: frames decodificados (y entregados al buzón)
    /// durante el episodio. Con `FreezeOnLoss` debería ser 0.
    pub episode_frames: Vec<u64>,
    /// Episodio abierto al terminar la sesión (sin IDR que lo cerrara).
    pub episode_open_at_end: bool,
    pub first_frame: Option<Duration>,
    pub last_decode_us: u32,
    // Presentación.
    pub frames_presented: u64,
    /// Presentados con posibles artefactos (decodificados con referencias rotas).
    pub frames_presented_concealed: u64,
    pub cursor_only_presents: u64,
    /// Huecos entre frames de vídeo nuevos presentados mayores que
    /// [`FREEZE_THRESHOLD`] (congelaciones visibles). Ojo: un emisor que solo
    /// envía cuando cambia la pantalla también produce huecos sin pérdidas.
    pub freezes: u64,
    pub frozen_total: Duration,
    pub longest_freeze: Duration,
    /// Decodificado (buzón) → `Present()` devuelto.
    pub present: LatencyStats,
    /// Frame reensamblado → `Present()` devuelto.
    pub recv_to_present: LatencyStats,
    pub present_cpu: LatencyStats,
    pub present_vblank: LatencyStats,
    /// Captura (pts QPC) → `Present()` devuelto; solo con `--same-host-latency`.
    pub capture_to_present: LatencyStats,
    pub capture_to_present_invalid: u64,
    pub video_size: (u32, u32),
    pub zero_copy: Option<bool>,
}

impl SessionStats {
    /// Informe final de la sesión (contrato §Métricas del receptor).
    pub fn report(&self, same_host: bool) {
        let elapsed = self
            .started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let fps = |n: u64| {
            if elapsed > 0.0 {
                n as f64 / elapsed
            } else {
                0.0
            }
        };
        println!("=== crossscreen-viewer: resumen de la sesión ===");
        println!(
            "duración {:.2} s, vídeo {}x{}, zero-copy {}",
            elapsed,
            self.video_size.0,
            self.video_size.1,
            match self.zero_copy {
                Some(true) => "sí",
                Some(false) => "no (copia GPU→GPU)",
                None => "sin frames",
            }
        );
        println!(
            "red: {} datagramas, {} frames reensamblados ({:.1} fps), {} perdidos ({} sin ningún datagrama), {} shards recuperados por FEC, {} datagramas tardíos, {} desbordes de la cola de decodificación",
            self.datagrams,
            self.frames_received,
            fps(self.frames_received),
            self.frames_lost,
            self.frames_missing,
            self.shards_recovered,
            self.late_datagrams,
            self.decode_queue_overflows
        );
        println!(
            "decodificación ({}): {} frames ({} con referencias rotas), {} errores, {} atascos, {} descartados esperando IDR, {} sustituidos en el buzón sin presentar",
            match self.loss_policy {
                LossPolicy::DecodeThrough => "decodificar a través de la pérdida",
                LossPolicy::FreezeOnLoss => "congelar hasta IDR",
            },
            self.frames_decoded,
            self.frames_decoded_concealed,
            self.decode_errors,
            self.decode_stalls,
            self.dropped_waiting_idr,
            self.mailbox_replaced,
        );
        println!(
            "RequestKeyframe: {} enviados ({} por espera vencida), {} avisos de pérdida sin petición nueva (ya había una pendiente), RTT {:.1} ms",
            self.keyframe_requests_sent,
            self.keyframe_request_timeouts,
            self.keyframe_needs_suppressed,
            self.rtt.as_secs_f64() * 1e3
        );
        println!(
            "presentación: {} frames ({:.1} fps, {} con posibles artefactos), {} repintados solo de cursor; cursor: {} formas, {} posiciones{}",
            self.frames_presented,
            fps(self.frames_presented),
            self.frames_presented_concealed,
            self.cursor_only_presents,
            self.cursor_shapes,
            self.cursor_positions,
            if self.input_events_sent > 0 {
                format!("; entrada remota enviada: {} eventos", self.input_events_sent)
            } else {
                String::new()
            }
        );
        println!(
            "congelaciones (> {} ms sin frame nuevo): {}, {:.0} ms en total, la más larga {:.0} ms",
            FREEZE_THRESHOLD.as_millis(),
            self.freezes,
            self.frozen_total.as_secs_f64() * 1e3,
            self.longest_freeze.as_secs_f64() * 1e3
        );
        let n = self.episode_requests.len();
        if n > 0 {
            let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
            println!(
                "episodios de pérdida cerrados: {n}{}; peticiones por episodio media {:.2} máx. {}; frames decodificados durante el episodio media {:.1} máx. {}",
                if self.episode_open_at_end { " (+1 abierto al terminar)" } else { "" },
                mean(&self.episode_requests),
                self.episode_requests.iter().max().copied().unwrap_or(0),
                mean(&self.episode_frames),
                self.episode_frames.iter().max().copied().unwrap_or(0),
            );
        } else if self.episode_open_at_end {
            println!("episodios de pérdida: 1 abierto al terminar, ninguno cerrado");
        }
        if let Some(t) = self.first_frame {
            println!(
                "primer frame decodificado: {:.1} ms tras crear el decodificador",
                t.as_secs_f64() * 1e3
            );
        }
        let line = |name: &str, s: &LatencyStats| match s.summary() {
            Some(sum) => println!("{name:<44} {sum}"),
            None => println!("{name:<44} (sin datos)"),
        };
        line("decode (AU→salida del MFT, CPU)", &self.decode);
        line(
            "presentación (decodificado→Present devuelto)",
            &self.present,
        );
        line("reensamblado→Present devuelto", &self.recv_to_present);
        line("Present() CPU", &self.present_cpu);
        line("Present()→vblank (DXGI stats)", &self.present_vblank);
        line(
            "recuperación (1.ª pérdida→IDR decodificado)",
            &self.recovery,
        );
        if same_host {
            line(
                "captura→Present devuelto (MISMO EQUIPO)",
                &self.capture_to_present,
            );
            if self.capture_to_present_invalid > 0 {
                println!(
                    "  ({} muestras descartadas: pts en el futuro, relojes no comparables)",
                    self.capture_to_present_invalid
                );
            }
        }
    }
}
