//! Hilo de red del receptor (tokio): endpoint QUIC con huellas fijadas, canal
//! de control según `docs/09-contrato-sesion-mvp.md`, reensamblado de los
//! datagramas de vídeo y reenvío al hilo de decodificación.
//!
//! Nunca se bloquea por la presentación: el vídeo pasa al decodificador con
//! `try_send` sobre una cola acotada (si se llena se trata como pérdida y se
//! pide IDR) y el cursor y los avisos van a la ventana con
//! `EventLoopProxy::send_event`, que no espera.

use crate::clock;
use crate::session::{DecodeInput, KeyframeSignal, SessionStats, StartSession, UiCmd, UserEvent};
use anyhow::{anyhow, bail, Context, Result};
use cs_core::{Codec, CodecSupport, H264Profile, Mode, Roles};
use cs_protocol::control::{
    ByeReason, ControlMessage, Hello, ReceiverStats, SinkCapabilities, StreamId,
};
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::trust::TrustStore;
use cs_security::verifier::PeerPolicy;
use cs_transport::{Connection, Endpoint, FramedStream, Reassembler, ReassemblyEvent};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;
use winit::event_loop::EventLoopProxy;

/// Único flujo de la sesión (contrato).
pub const STREAM_ID: StreamId = 1;
/// AU comprimidas en cola hacia el decodificador. Holgado para una ráfaga
/// tras un hipo de red; si se llena, el decodificador va muy por detrás.
const DECODE_QUEUE: usize = 16;
/// Mínimo entre dos `RequestKeyframe` (contrato: como máximo uno cada 100 ms).
pub const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Espera máxima de cada paso del arranque de sesión.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuración del hilo de red.
#[derive(Debug, Clone)]
pub struct NetConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub once: bool,
    pub native_mode: Mode,
    pub dpi: u16,
}

/// Arranca el hilo de red. Al terminar envía `UserEvent::NetDone`.
pub fn spawn(
    cfg: NetConfig,
    proxy: EventLoopProxy<UserEvent>,
    cmd_rx: UnboundedReceiver<UiCmd>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("cs-net".into())
        .spawn(move || {
            let res = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("cs-net-worker")
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(run(cfg, proxy.clone(), cmd_rx)));
            let _ = proxy.send_event(UserEvent::NetDone(res.err().map(|e| format!("{e:#}"))));
        })
}

/// Coalescencia de `RequestKeyframe`: como máximo uno cada
/// [`KEYFRAME_MIN_INTERVAL`]; las peticiones intermedias no se pierden, se
/// envían al cumplirse el intervalo.
#[derive(Debug, Default)]
pub struct KeyframeLimiter {
    last_sent: Option<Instant>,
    pending: bool,
}

impl KeyframeLimiter {
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// Llegó un IDR: la petición pendiente sobra.
    pub fn cancel(&mut self) {
        self.pending = false;
    }

    /// `true` si hay que enviar ahora (y lo da por enviado).
    pub fn due(&mut self, now: Instant) -> bool {
        let ok = self.pending
            && self
                .last_sent
                .is_none_or(|t| now.duration_since(t) >= KEYFRAME_MIN_INTERVAL);
        if ok {
            self.pending = false;
            self.last_sent = Some(now);
        }
        ok
    }
}

/// Detecta huecos en `frame_seq` (consecutivo según el contrato). El
/// `Reassembler` solo declara perdidos los frames de los que recibió algún
/// fragmento; si se pierden **todos** los datagramas de un frame (habitual
/// en P-frames de uno o dos datagramas), solo se nota por el hueco.
#[derive(Debug, Default)]
pub struct SeqTracker {
    next: Option<u32>,
}

impl SeqTracker {
    /// Registra un frame entregado o perdido. Devuelve `(primer_perdido, n)`
    /// si faltan frames entre el anterior y este. Los eventos llegan en orden
    /// creciente; un `seq` anterior al esperado se ignora.
    pub fn observe(&mut self, seq: u32) -> Option<(u32, u32)> {
        let gap = match self.next {
            Some(n) => {
                let d = seq.wrapping_sub(n);
                if d >= 1 << 31 {
                    return None; // anterior al esperado
                }
                (d > 0).then_some((n, d))
            }
            None => None,
        };
        self.next = Some(seq.wrapping_add(1));
        gap
    }
}

/// *Jitter* de llegada de frames al estilo RFC 3550 (§A.8): compara la
/// diferencia entre llegadas con la diferencia entre `pts`. El desfase
/// absoluto entre relojes se cancela, así que vale entre equipos distintos.
#[derive(Debug, Default)]
pub struct Jitter {
    prev_transit: Option<i64>,
    jitter_us: f64,
}

impl Jitter {
    pub fn update(&mut self, pts_us: u32, arrival_us: u32) {
        let transit = i64::from(arrival_us.wrapping_sub(pts_us) as i32);
        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).abs() as f64;
            self.jitter_us += (d - self.jitter_us) / 16.0;
        }
        self.prev_transit = Some(transit);
    }

    pub fn get_us(&self) -> u32 {
        self.jitter_us as u32
    }
}

enum Exit {
    /// La sesión terminó (el emisor cerró, error...): volver a escuchar.
    Ended(String),
    /// Cierre pedido desde la ventana (`--seconds`, cerrar la ventana, Esc).
    Shutdown,
}

async fn run(
    cfg: NetConfig,
    proxy: EventLoopProxy<UserEvent>,
    mut cmd_rx: UnboundedReceiver<UiCmd>,
) -> Result<()> {
    let identity = Identity::load_or_create(&cfg.data_dir).context("cargando identidad")?;
    let trust = TrustStore::open(cfg.data_dir.join("trusted-peers.json"))
        .context("abriendo almacén de confianza")?;
    let fps = trust.fingerprints();
    if fps.is_empty() {
        bail!(
            "no hay equipos emparejados en {}: empareja primero con `crossscreen --data-dir {} pair-listen` \
             en este equipo y `crossscreen pair <ip:puerto> <PIN>` en el emisor",
            cfg.data_dir.display(),
            cfg.data_dir.display()
        );
    }
    let policy = PeerPolicy::pinned(fps);
    // Conjunto compartido con el verificador TLS: se actualiza antes de cada
    // conexión para que un `crossscreen forget` surta efecto sin reiniciar.
    let PeerPolicy::Pinned(pinned_set) = policy.clone() else {
        unreachable!("PeerPolicy::pinned crea la variante Pinned")
    };
    let endpoint = Endpoint::server(cfg.bind, identity.clone(), policy, ALPN)
        .with_context(|| format!("escuchando en {}", cfg.bind))?;
    let trust_path = cfg.data_dir.join("trusted-peers.json");
    let current_trust = || -> Result<Vec<cs_security::Fingerprint>> {
        Ok(TrustStore::open(&trust_path)
            .context("releyendo el almacén de confianza")?
            .fingerprints())
    };
    println!(
        "crossscreen-viewer escuchando en UDP {} (huella {}, {} par(es) de confianza); native_mode {}x{}@{:.2} Hz, {} dpi",
        endpoint.local_addr()?,
        identity.fingerprint().short(),
        trust.fingerprints().len(),
        cfg.native_mode.width,
        cfg.native_mode.height,
        f64::from(cfg.native_mode.refresh_mhz) / 1000.0,
        cfg.dpi
    );
    loop {
        // Revocación: releer el almacén antes de aceptar la siguiente conexión.
        match current_trust() {
            Ok(fps) => {
                if let Ok(mut set) = pinned_set.write() {
                    *set = fps.into_iter().collect();
                }
            }
            Err(e) => eprintln!("{e:#}; se mantiene la lista anterior"),
        }
        let conn = tokio::select! {
            c = endpoint.accept() => match c {
                None => break,
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    eprintln!("conexión rechazada: {e}");
                    continue;
                }
            },
            _ = cmd_rx.recv() => break,
        };
        // Segunda comprobación tras el handshake: el par pudo ser olvidado
        // mientras se esperaba la conexión. Si no se puede releer, se rechaza.
        let still_trusted = current_trust()
            .map(|fps| fps.contains(&conn.peer_fingerprint()))
            .unwrap_or(false);
        if !still_trusted {
            eprintln!(
                "conexión de {} rechazada: el equipo {} ya no está emparejado",
                conn.remote_addr(),
                conn.peer_fingerprint().short()
            );
            conn.close(b"not trusted");
            continue;
        }
        println!(
            "conexión de {} ({})",
            conn.remote_addr(),
            conn.peer_fingerprint().short()
        );
        let exit = session(&conn, &identity, &cfg, &proxy, &mut cmd_rx).await;
        let shutdown = match exit {
            Ok(Exit::Shutdown) => {
                println!("sesión cerrada a petición del receptor");
                true
            }
            Ok(Exit::Ended(reason)) => {
                println!("sesión terminada: {reason}");
                false
            }
            Err(e) => {
                eprintln!("sesión fallida: {e:#}");
                false
            }
        };
        conn.close(b"bye");
        if shutdown || cfg.once {
            break;
        }
        println!("volviendo a escuchar…");
    }
    endpoint.close();
    // Dar tiempo a que salga el CONNECTION_CLOSE.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

async fn timed<T, E: Into<anyhow::Error>>(
    what: &str,
    f: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, f).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(anyhow!("tiempo agotado esperando {what}")),
    }
}

/// Arranque de sesión (pasos 1–5 del contrato) y bucle de recepción.
async fn session(
    conn: &Connection,
    identity: &Identity,
    cfg: &NetConfig,
    proxy: &EventLoopProxy<UserEvent>,
    cmd_rx: &mut UnboundedReceiver<UiCmd>,
) -> Result<Exit> {
    let mut ctl = FramedStream::new(timed("el canal de control", conn.accept_bi()).await?);
    // 1. Hello de la fuente.
    let hello: ControlMessage = timed("Hello", ctl.expect()).await?;
    let ControlMessage::Hello(h) = hello else {
        bail!("primer mensaje inesperado: {hello:?}");
    };
    if !PROTOCOL_VERSION.is_compatible_with(&h.version) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::IncompatibleVersion,
            })
            .await;
        ctl.finish();
        bail!(
            "versión incompatible: la fuente usa {}.{}, este visor {}.{}",
            h.version.major,
            h.version.minor,
            PROTOCOL_VERSION.major,
            PROTOCOL_VERSION.minor
        );
    }
    println!(
        "fuente: «{}» ({}), protocolo {}.{}",
        h.device_name, h.device_id, h.version.major, h.version.minor
    );
    // 2. Hello propio.
    ctl.send(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        device_id: identity.fingerprint().device_id(),
        device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "crossscreen-viewer".into()),
        roles: Roles {
            source: false,
            sink: true,
        },
    }))
    .await?;
    // 3. Capacidades.
    ctl.send(&ControlMessage::SinkCapabilities(SinkCapabilities {
        decoders: vec![CodecSupport {
            codec: Codec::H264,
            max_h264_profile: Some(H264Profile::High),
            hardware: true,
            // Verificado hasta 2160p60 en la PoC-5 (Iris Xe).
            max_pixel_rate: 3840 * 2160 * 60,
        }],
        native_mode: cfg.native_mode,
        dpi: cfg.dpi,
    }))
    .await?;
    // 4. StartStream (los mensajes de cursor previos se guardan para después).
    let mut early_shape = None;
    let mut early_pos = None;
    let start = loop {
        let msg: ControlMessage = timed("StartStream", ctl.expect()).await?;
        match msg {
            ControlMessage::StartStream(s) => break s,
            ControlMessage::CursorShape(c) => early_shape = Some(c),
            ControlMessage::CursorPosition { x, y, visible, .. } => {
                early_pos = Some((x, y, visible))
            }
            ControlMessage::Bye { reason } => {
                return Ok(Exit::Ended(format!("Bye antes de empezar: {reason:?}")))
            }
            other => tracing::debug!("antes de StartStream: {other:?}"),
        }
    };
    let m = start.display_mode;
    let mode_ok = (Mode::MIN_DIM..=Mode::MAX_DIM).contains(&m.width)
        && (Mode::MIN_DIM..=Mode::MAX_DIM).contains(&m.height);
    if start.stream_id != STREAM_ID || start.codec != Codec::H264 || !mode_ok {
        let reason = format!(
            "no soportado: stream_id {} (se espera {STREAM_ID}), códec {:?}, modo {}x{}",
            start.stream_id, start.codec, m.width, m.height
        );
        ctl.send(&ControlMessage::StreamFailed {
            stream_id: start.stream_id,
            reason: reason.clone(),
        })
        .await?;
        ctl.finish();
        bail!(reason);
    }
    println!(
        "StartStream: {:?}, {}x{}@{:.2} Hz, {} kb/s",
        start.mode,
        m.width,
        m.height,
        f64::from(m.refresh_mhz) / 1000.0,
        start.bitrate_kbps
    );

    // 5. Ventana + decodificador en el hilo de la ventana.
    let (decode_tx, decode_rx) = std::sync::mpsc::sync_channel(DECODE_QUEUE);
    let (keyframe_tx, keyframe_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let stats = Arc::new(Mutex::new(SessionStats::default()));
    proxy
        .send_event(UserEvent::Start(
            Box::new(StartSession {
                display_mode: m,
                peer: format!("{} ({})", h.device_name, conn.peer_fingerprint().short()),
                decode_rx,
                keyframe_tx,
                stats: stats.clone(),
            }),
            ready_tx,
        ))
        .map_err(|_| anyhow!("el bucle de la ventana ya no existe"))?;
    // A partir de aquí la ventana tiene una sesión: siempre hay que mandar End.
    let exit = streaming(
        conn,
        &mut ctl,
        proxy,
        cmd_rx,
        ready_rx,
        decode_tx,
        keyframe_rx,
        &stats,
        early_shape,
        early_pos,
    )
    .await;
    ctl.finish();
    let reason = match &exit {
        Ok(Exit::Ended(r)) => r.clone(),
        Ok(Exit::Shutdown) => "cierre pedido por el receptor".into(),
        Err(e) => format!("{e:#}"),
    };
    let _ = proxy.send_event(UserEvent::End { reason });
    exit
}

#[allow(clippy::too_many_arguments)]
async fn streaming(
    conn: &Connection,
    ctl: &mut FramedStream,
    proxy: &EventLoopProxy<UserEvent>,
    cmd_rx: &mut UnboundedReceiver<UiCmd>,
    ready_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
    decode_tx: SyncSender<DecodeInput>,
    mut keyframe_rx: UnboundedReceiver<KeyframeSignal>,
    stats: &Arc<Mutex<SessionStats>>,
    early_shape: Option<cs_protocol::control::CursorShape>,
    early_pos: Option<(i32, i32, bool)>,
) -> Result<Exit> {
    match timed("la ventana y el decodificador", async {
        ready_rx
            .await
            .map_err(|_| anyhow!("la ventana no respondió"))
    })
    .await?
    {
        Ok(()) => {}
        Err(e) => {
            let _ = ctl
                .send(&ControlMessage::StreamFailed {
                    stream_id: STREAM_ID,
                    reason: e.clone(),
                })
                .await;
            bail!("no se pudo preparar la presentación: {e}");
        }
    }
    ctl.send(&ControlMessage::StreamStarted {
        stream_id: STREAM_ID,
    })
    .await?;
    println!("StreamStarted enviado; esperando el primer IDR");
    let lock = || stats.lock().unwrap_or_else(|e| e.into_inner());
    lock().started = Some(Instant::now());
    if let Some(c) = early_shape {
        if c.validate() {
            lock().cursor_shapes += 1;
            let _ = proxy.send_event(UserEvent::CursorShape(c));
        }
    }
    if let Some((x, y, visible)) = early_pos {
        let _ = proxy.send_event(UserEvent::CursorPosition { x, y, visible });
    }

    let mut reasm = Reassembler::new(STREAM_ID);
    let mut keyframes = KeyframeLimiter::default();
    let mut jitter = Jitter::default();
    let mut seqs = SeqTracker::default();
    // Frame perdido pendiente de avisar al decodificador (cola llena).
    let mut overflow_lost: Option<u32> = None;
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut kf_tick = tokio::time::interval(Duration::from_millis(10));
    kf_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let exit = 'main: loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let d = match d {
                    Ok(d) => d,
                    Err(e) => break Exit::Ended(format!("conexión cerrada: {e}")),
                };
                let events = match reasm.push(&d) {
                    Ok(ev) => ev,
                    Err(e) => {
                        tracing::debug!("datagrama inválido: {e}");
                        continue;
                    }
                };
                let now = Instant::now();
                for ev in events {
                    let seq = match &ev {
                        ReassemblyEvent::Frame(f) => f.frame_seq,
                        ReassemblyEvent::Lost { frame_seq } => *frame_seq,
                    };
                    // Frames de los que no llegó ningún datagrama: el
                    // reensamblador no los ve, pero `frame_seq` es consecutivo.
                    let gap = seqs.observe(seq);
                    let mut inputs = Vec::with_capacity(2);
                    if let Some((first, n)) = gap {
                        tracing::debug!("hueco de {n} frame(s) desde {first}");
                        lock().frames_missing += u64::from(n);
                        inputs.push(DecodeInput::Lost { frame_seq: first, at: now });
                    }
                    inputs.push(match ev {
                        ReassemblyEvent::Frame(f) => {
                            jitter.update(f.pts_us, clock::qpc_us32());
                            DecodeInput::Frame { frame: f, received_at: now }
                        }
                        ReassemblyEvent::Lost { frame_seq } => {
                            tracing::debug!("reensamblador: frame {frame_seq} perdido");
                            DecodeInput::Lost { frame_seq, at: now }
                        }
                    });
                    for input in inputs {
                        if let Some(lost) = overflow_lost {
                            match decode_tx.try_send(DecodeInput::Lost { frame_seq: lost, at: now }) {
                                Ok(()) => overflow_lost = None,
                                Err(TrySendError::Full(_)) => {
                                    lock().decode_queue_overflows += 1;
                                    continue;
                                }
                                Err(TrySendError::Disconnected(_)) => {
                                    break 'main Exit::Ended("el decodificador terminó".into())
                                }
                            }
                        }
                        match decode_tx.try_send(input) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                lock().decode_queue_overflows += 1;
                                overflow_lost = Some(seq);
                                keyframes.request();
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                break 'main Exit::Ended("el decodificador terminó".into())
                            }
                        }
                    }
                }
            }
            m = ctl.recv::<ControlMessage>() => match m {
                Ok(Some(ControlMessage::CursorShape(c))) => {
                    if c.stream_id == STREAM_ID && c.validate() {
                        lock().cursor_shapes += 1;
                        let _ = proxy.send_event(UserEvent::CursorShape(c));
                    } else {
                        tracing::warn!("CursorShape inválido ({}x{}, {} bytes)", c.width, c.height, c.rgba.len());
                    }
                }
                Ok(Some(ControlMessage::CursorPosition { stream_id, x, y, visible })) => {
                    if stream_id == STREAM_ID {
                        lock().cursor_positions += 1;
                        let _ = proxy.send_event(UserEvent::CursorPosition { x, y, visible });
                    }
                }
                Ok(Some(ControlMessage::Bye { reason })) => break Exit::Ended(format!("Bye de la fuente ({reason:?})")),
                Ok(Some(ControlMessage::StopStream { .. })) => break Exit::Ended("StopStream de la fuente".into()),
                Ok(Some(other)) => tracing::debug!("control: {other:?}"),
                Ok(None) => break Exit::Ended("la fuente cerró el canal de control".into()),
                Err(e) => break Exit::Ended(format!("canal de control: {e}")),
            },
            Some(sig) = keyframe_rx.recv() => {
                match sig {
                    KeyframeSignal::Needed => keyframes.request(),
                    KeyframeSignal::Received => keyframes.cancel(),
                }
                if keyframes.due(Instant::now()) {
                    send_keyframe_request(ctl, stats).await?;
                }
            }
            _ = kf_tick.tick() => {
                if keyframes.due(Instant::now()) {
                    send_keyframe_request(ctl, stats).await?;
                }
            }
            _ = stats_tick.tick() => {
                let r = reasm.stats();
                let (decode_us, missing) = {
                    let s = lock();
                    (s.last_decode_us, s.frames_missing)
                };
                // Estimación: shards de datos reconstruidos por FEC respecto a
                // los datagramas recibidos (la paridad perdida no se ve).
                let pre_fec_bp = (r.shards_recovered * 10_000 / r.datagrams.max(1)).min(10_000) as u16;
                let msg = ControlMessage::ReceiverStats(ReceiverStats {
                    stream_id: STREAM_ID,
                    frames_received: r.frames_delivered as u32,
                    frames_lost: (r.frames_lost + missing) as u32,
                    pre_fec_loss_bp: pre_fec_bp,
                    jitter_us: jitter.get_us(),
                    decode_time_us: decode_us,
                });
                ctl.send(&msg).await?;
            }
            _ = cmd_rx.recv() => {
                let _ = ctl.send(&ControlMessage::Bye { reason: ByeReason::UserRequested }).await;
                break Exit::Shutdown;
            }
        }
    };
    let r = reasm.stats();
    let mut s = lock();
    s.datagrams = r.datagrams;
    s.frames_received = r.frames_delivered;
    s.frames_lost = r.frames_lost + s.frames_missing;
    s.shards_recovered = r.shards_recovered;
    s.late_datagrams = r.late_datagrams;
    drop(s);
    // `decode_tx` se suelta al volver: el hilo de decodificación termina.
    drop(decode_tx);
    Ok(exit)
}

async fn send_keyframe_request(
    ctl: &mut FramedStream,
    stats: &Arc<Mutex<SessionStats>>,
) -> Result<()> {
    ctl.send(&ControlMessage::RequestKeyframe {
        stream_id: STREAM_ID,
    })
    .await?;
    stats
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keyframe_requests_sent += 1;
    tracing::debug!("RequestKeyframe enviado");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_requests_are_coalesced() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::default();
        assert!(!k.due(t0), "sin petición no se envía nada");
        k.request();
        assert!(k.due(t0));
        k.request();
        k.request();
        assert!(!k.due(t0 + Duration::from_millis(50)), "dentro de 100 ms");
        assert!(
            k.due(t0 + Duration::from_millis(100)),
            "la petición pendiente sale al cumplirse el intervalo"
        );
        assert!(
            !k.due(t0 + Duration::from_millis(500)),
            "ya no hay pendiente"
        );
        k.request();
        k.cancel();
        assert!(
            !k.due(t0 + Duration::from_secs(1)),
            "cancelada al llegar el IDR"
        );
    }

    #[test]
    fn seq_gaps_detected() {
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(0), None);
        assert_eq!(t.observe(1), None);
        assert_eq!(t.observe(4), Some((2, 2)));
        assert_eq!(t.observe(3), None, "anterior: ignorado");
        assert_eq!(t.observe(5), None);
        let mut w = SeqTracker::default();
        w.observe(u32::MAX - 1);
        assert_eq!(w.observe(1), Some((u32::MAX, 2)), "vuelta de u32");
    }

    #[test]
    fn jitter_zero_for_regular_arrivals_and_wraps() {
        let mut j = Jitter::default();
        // Llegadas exactamente al ritmo de los pts, con desfase arbitrario y
        // cruzando la vuelta de 2^32.
        let base = u32::MAX - 20_000;
        for i in 0..10u32 {
            let pts = base.wrapping_add(i * 16_667);
            j.update(pts, pts.wrapping_add(123_456));
        }
        assert_eq!(j.get_us(), 0);
        j.update(
            base.wrapping_add(10 * 16_667),
            base.wrapping_add(10 * 16_667 + 123_456 + 1_600),
        );
        assert_eq!(j.get_us(), 100);
    }
}
