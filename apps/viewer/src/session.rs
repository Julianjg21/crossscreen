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
    /// Hace falta un IDR (pérdida, error o primer frame no IDR).
    Needed,
    /// Ya se decodificó un IDR: cancelar la petición pendiente, si la hay.
    Received,
}

/// Órdenes del hilo de la ventana al de red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    /// Cerrar la sesión en curso (si la hay) con `Bye` y dejar de escuchar.
    Shutdown,
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

/// Métricas de una sesión (las escriben los tres hilos).
#[derive(Debug, Default)]
pub struct SessionStats {
    pub started: Option<Instant>,
    // Red.
    pub datagrams: u64,
    pub frames_received: u64,
    /// Perdidos en total: los que declara el reensamblador + `frames_missing`.
    pub frames_lost: u64,
    /// Frames sin ningún datagrama recibido (huecos en `frame_seq`).
    pub frames_missing: u64,
    pub shards_recovered: u64,
    pub late_datagrams: u64,
    pub decode_queue_overflows: u64,
    pub keyframe_requests_sent: u64,
    pub cursor_shapes: u64,
    pub cursor_positions: u64,
    // Decodificación.
    pub frames_decoded: u64,
    pub decode_errors: u64,
    pub dropped_waiting_idr: u64,
    pub mailbox_replaced: u64,
    pub decode: LatencyStats,
    /// Desde el primer `Lost`/error hasta el primer IDR decodificado.
    pub recovery: LatencyStats,
    pub first_frame: Option<Duration>,
    pub last_decode_us: u32,
    // Presentación.
    pub frames_presented: u64,
    pub cursor_only_presents: u64,
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
            "decodificación: {} frames, {} errores, {} descartados esperando IDR, {} sustituidos en el buzón sin presentar, {} RequestKeyframe enviados",
            self.frames_decoded,
            self.decode_errors,
            self.dropped_waiting_idr,
            self.mailbox_replaced,
            self.keyframe_requests_sent
        );
        println!(
            "presentación: {} frames ({:.1} fps), {} repintados solo de cursor; cursor: {} formas, {} posiciones",
            self.frames_presented,
            fps(self.frames_presented),
            self.cursor_only_presents,
            self.cursor_shapes,
            self.cursor_positions
        );
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
        line("recuperación (pérdida→IDR decodificado)", &self.recovery);
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
