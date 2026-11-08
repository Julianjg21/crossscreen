//! Sesión emisora: canal de control (contrato §"Canal de control"), monitor
//! virtual, hilo de captura + codificación y tarea de envío de vídeo.

use crate::display_guard::{mode_candidates, VirtualDisplayGuard};
use crate::stats::{ControlStats, SendStats, SenderReport};
use crate::timing::{wire_pts, KeyframeCoalescer, KeyframeDecision};
use crate::win::worker::{self, CursorPos, VideoUnit, WorkerControl, WorkerOutputs, WorkerParams};
use crate::{SenderConfig, STREAM_ID};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use cs_capture::windows_dxgi::{list_outputs, poll_cursor_in_rect, qpc_now_us};
use cs_capture::{CaptureTarget, Rect};
use cs_core::{Codec, Mode, Roles, StreamMode};
use cs_input::{InputInjector, ScreenRect, SendInputInjector};
use cs_protocol::control::{
    ByeReason, ControlMessage, CursorShape, Hello, SinkCapabilities, StartStream,
};
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::verifier::PeerPolicy;
use cs_transport::fec::{AdaptiveFec, AdaptiveFecConfig};
use cs_transport::pacer::{self, PacedSendOptions, Pacer, PacerConfig};
use cs_transport::packetizer::FrameMeta;
use cs_transport::simloss::LossInjector;
use cs_transport::{Connection, FramedStream, Packetizer, PacketizerConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// El receptor crea decodificador y ventana antes de `StreamStarted`.
const START_TIMEOUT: Duration = Duration::from_secs(15);
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Hilo de captura + codificación en marcha. Al soltarlo se para y se
/// espera (antes de retirar el monitor: el orden de declaración importa).
struct WorkerHandle {
    ctl: Arc<WorkerControl>,
    join: Option<std::thread::JoinHandle<crate::stats::WorkerStats>>,
}

impl WorkerHandle {
    async fn stop(mut self) -> crate::stats::WorkerStats {
        self.ctl.stop.store(true, Ordering::Relaxed);
        let Some(j) = self.join.take() else {
            return Default::default();
        };
        match tokio::task::spawn_blocking(move || j.join()).await {
            Ok(Ok(st)) => st,
            _ => crate::stats::WorkerStats {
                fatal: Some("el hilo de captura+codificación terminó con pánico".into()),
                ..Default::default()
            },
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

async fn recv_msg(ctl: &mut FramedStream, what: &'static str) -> Result<ControlMessage> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, ctl.expect::<ControlMessage>())
        .await
        .with_context(|| format!("el receptor no envió {what} a tiempo"))?
        .with_context(|| format!("esperando {what}"))
}

pub(crate) async fn run(
    mut conn: Connection,
    cfg: SenderConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<SenderReport> {
    let mut ctl = FramedStream::new(conn.open_bi().await.context("abriendo canal de control")?);

    // 1-3. Hello ⇄ Hello, SinkCapabilities.
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
    let peer = match recv_msg(&mut ctl, "Hello").await? {
        ControlMessage::Hello(h) => h,
        ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
        other => bail!("se esperaba Hello y llegó {other:?}"),
    };
    if !PROTOCOL_VERSION.is_compatible_with(&peer.version) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::IncompatibleVersion,
            })
            .await;
        ctl.finish();
        bail!(
            "versión de protocolo incompatible: local {:?}, receptor {:?}",
            PROTOCOL_VERSION,
            peer.version
        );
    }
    println!(
        "Receptor: '{}' ({}) protocolo {}.{}",
        peer.device_name, peer.device_id, peer.version.major, peer.version.minor
    );
    let sink: SinkCapabilities = loop {
        match recv_msg(&mut ctl, "SinkCapabilities").await? {
            ControlMessage::SinkCapabilities(c) => break c,
            ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
            other => tracing::debug!(?other, "mensaje ignorado antes de SinkCapabilities"),
        }
    };
    println!(
        "Receptor: modo nativo {}x{}@{} Hz, {} dpi, decodificadores {:?}",
        sink.native_mode.width,
        sink.native_mode.height,
        sink.native_mode.refresh_mhz / 1000,
        sink.dpi,
        sink.decoders
            .iter()
            .map(|d| (d.codec, d.hardware))
            .collect::<Vec<_>>()
    );
    if !sink.decoders.iter().any(|d| d.codec == Codec::H264) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::Error,
            })
            .await;
        bail!("el receptor no anuncia decodificador H.264");
    }

    // Monitor: virtual (extend) o principal (duplicate). La guardia se
    // declara ANTES que el hilo para que se suelte DESPUÉS de pararlo.
    let (guard, target, display_mode) = match cfg.mode {
        StreamMode::Extend => {
            let cands = mode_candidates(cfg.requested_mode, sink.native_mode, cfg.fallback_mode);
            let guard =
                tokio::task::spawn_blocking(move || VirtualDisplayGuard::show_first(&cands))
                    .await?
                    .context("mostrando el monitor virtual")?;
            let shown = guard
                .shown()
                .cloned()
                .context("monitor virtual sin datos")?;
            println!(
                "Monitor virtual {} mostrado a {}x{}@{} Hz",
                shown.gdi_name,
                shown.mode.width,
                shown.mode.height,
                shown.mode.refresh_mhz / 1000
            );
            (
                Some(guard),
                CaptureTarget::GdiName(shown.gdi_name.clone()),
                shown.mode,
            )
        }
        StreamMode::Duplicate => {
            let primary = list_outputs()?
                .into_iter()
                .find(|o| CaptureTarget::Primary.matches(o))
                .context("no se encontró la pantalla principal")?;
            let hz = cfg
                .requested_mode
                .map_or(60, |m| (m.refresh_mhz / 1000).max(1));
            // El tamaño real (píxeles físicos) lo da la duplicación al
            // arrancar el hilo; `desktop_rect` puede venir en píxeles lógicos
            // si el proceso no es DPI-aware.
            let r = primary.desktop_rect;
            let mode = Mode::new(r.width() as u32, r.height() as u32, hz);
            println!("Duplicando la pantalla principal {}", primary.gdi_name);
            (None, CaptureTarget::Primary, mode)
        }
    };
    let fps = ((display_mode.refresh_mhz + 500) / 1000).max(1);
    let mut cfg = cfg;
    cfg.bitrate_kbps = effective_bitrate_kbps(cfg.bitrate_kbps, display_mode);

    // Hilo de captura + codificación (arranca ya; codifica tras StreamStarted).
    // Capacidad 2: la segunda plaza se reserva para IDR forzados, mientras que
    // los P-frames siguen limitados a 1 en vuelo.
    let (video_tx, video_rx) = mpsc::channel::<VideoUnit>(2);
    let (pos_tx, mut pos_rx) = watch::channel::<Option<CursorPos>>(None);
    let (shape_tx, mut shape_rx) = watch::channel::<Option<Arc<CursorShape>>>(None);
    let (rect_tx, rect_rx) = watch::channel::<Option<Rect>>(None);
    let wctl = Arc::new(WorkerControl::default());
    let (join, ready, mut worker_done) = worker::spawn(
        WorkerParams {
            target,
            fps,
            bitrate_kbps: cfg.bitrate_kbps,
            stream_id: STREAM_ID,
            static_refresh: cfg.static_refresh,
            intra_refresh: cfg.intra_refresh,
        },
        WorkerOutputs {
            video: video_tx,
            cursor_pos: pos_tx.clone(),
            cursor_shape: shape_tx,
            desktop_rect: rect_tx,
        },
        wctl.clone(),
    )?;
    let worker = WorkerHandle {
        ctl: wctl.clone(),
        join: Some(join),
    };
    let ready = ready
        .await
        .unwrap_or_else(|_| Err("el hilo terminó sin informar".into()));
    let (cw, ch) = match ready {
        Ok(s) => s,
        Err(e) => {
            let _ = ctl
                .send(&ControlMessage::Bye {
                    reason: ByeReason::Error,
                })
                .await;
            ctl.finish();
            drop(worker);
            if let Some(g) = guard {
                let _ = tokio::task::spawn_blocking(move || g.hide()).await;
            }
            bail!("{e}");
        }
    };
    // En duplicate, el modo real es el de la duplicación (píxeles físicos).
    let display_mode = if cw > 0 {
        Mode {
            width: cw,
            height: ch,
            refresh_mhz: display_mode.refresh_mhz,
        }
    } else {
        display_mode
    };

    // 4-5. StartStream → StreamStarted.
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: STREAM_ID,
        mode: cfg.mode,
        codec: Codec::H264,
        display_mode,
        bitrate_kbps: cfg.bitrate_kbps,
    }))
    .await?;
    let started = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            match ctl.expect::<ControlMessage>().await? {
                ControlMessage::StreamStarted { stream_id } if stream_id == STREAM_ID => {
                    return Ok(())
                }
                ControlMessage::StreamFailed { reason, .. } => {
                    bail!("el receptor no pudo iniciar el flujo: {reason}")
                }
                ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
                other => tracing::debug!(?other, "mensaje ignorado antes de StreamStarted"),
            }
        }
    })
    .await;
    match started {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => bail!("el receptor no confirmó StreamStarted en {START_TIMEOUT:?}"),
    }
    println!(
        "Flujo iniciado: {}x{}@{fps} · {} kbps · FEC P={} IDR={} (adaptativo={}, entrelazado={}) · pacing {:?}{}",
        display_mode.width,
        display_mode.height,
        cfg.bitrate_kbps,
        cfg.fec_ratio,
        cfg.keyframe_fec_ratio,
        cfg.adaptive_fec,
        cfg.fec_interleave,
        cfg.pacing,
        cfg.sim_loss
            .map(|(m, seed)| format!(" · sim-loss {m} (seed={seed:#x})"))
            .unwrap_or_default()
    );
    // 6. El primer frame tras StreamStarted es IDR (el hilo arranca con need_idr).
    wctl.force_idr.store(true, Ordering::Relaxed);
    wctl.streaming.store(true, Ordering::Relaxed);
    let start = Instant::now();
    let frame_interval = Duration::from_micros(1_000_000 / u64::from(fps));

    let (fec_tx, fec_rx) = watch::channel(cfg.fec_ratio);
    let (mut stop_video_tx, stop_video_rx) = watch::channel(false);
    let mut video = tokio::spawn(video_task(
        conn.clone(),
        video_rx,
        fec_rx.clone(),
        stop_video_rx,
        cfg.clone(),
        frame_interval,
        SendStats::default(),
    ));

    // Bucle de control.
    let mut adaptive_fec = (cfg.adaptive_fec && cfg.fec_ratio > 0.0).then(|| {
        AdaptiveFec::new(AdaptiveFecConfig {
            initial: cfg.fec_ratio,
            ..AdaptiveFecConfig::default()
        })
    });
    let initial_fec = adaptive_fec
        .as_ref()
        .map_or(cfg.fec_ratio, AdaptiveFec::ratio);
    let _ = fec_tx.send(initial_fec);
    let mut cs = ControlStats {
        fec_initial: initial_fec,
        fec_final: initial_fec,
        fec_min: initial_fec,
        fec_max: initial_fec,
        keyframe_fec: cfg.keyframe_fec_ratio,
        fec_interleaved: cfg.fec_interleave,
        ..ControlStats::default()
    };
    let mut coalescer = KeyframeCoalescer::new(KEYFRAME_MIN_INTERVAL);
    coalescer.note_forced(start);
    let deadline = cfg.duration.map(|d| tokio::time::Instant::now() + d);
    let mut shutdown = shutdown;
    // Sondeo Win32 del cursor a 125 Hz (8 ms): mantiene el cursor fluido
    // aunque el hilo de captura esté bloqueado en `MfH264Encoder::encode` o
    // el monitor virtual no dispare eventos DXGI de puntero.
    let mut cursor_tick = tokio::time::interval(Duration::from_millis(8));
    cursor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Enviar el estado inicial del cursor si ya lo hay.
    pos_rx.mark_changed();
    shape_rx.mark_changed();
    let (mut pos_alive, mut shape_alive) = (true, true);
    let mut peer_gone = false;
    let _ = cs_input::make_process_per_monitor_dpi_aware();
    let mut injector = SendInputInjector::new();
    let end_reason: String = 'session: loop {
        let drop_reason: String = loop {
            let deferred = coalescer
                .deferred_until()
                .map(tokio::time::Instant::from_std);
            tokio::select! {
                r = shutdown.changed() => {
                    if r.is_err() || *shutdown.borrow() {
                        break 'session "parada pedida (Ctrl+C)".into();
                    }
                }
                _ = sleep_opt(deadline) => break 'session "duración cumplida (--seconds)".into(),
                _ = sleep_opt(deferred) => {
                    if coalescer.poll(Instant::now()) {
                        wctl.force_idr.store(true, Ordering::Relaxed);
                        cs.keyframes_forced += 1;
                    }
                }
                _ = cursor_tick.tick() => {
                    if let Some(rect) = *rect_rx.borrow() {
                        if let Some((x, y, visible)) = poll_cursor_in_rect(rect) {
                            let p = CursorPos { x, y, visible };
                            pos_tx.send_if_modified(|old| {
                                let changed = *old != Some(p);
                                *old = Some(p);
                                changed
                            });
                        }
                    }
                }
                _ = &mut worker_done => break 'session "fallo en captura/codificación".into(),
                m = ctl.recv::<ControlMessage>() => match m {
                    Ok(Some(ControlMessage::RequestKeyframe { .. })) => {
                        cs.keyframe_requests += 1;
                        match coalescer.request_with_rtt(Instant::now(), conn.rtt()) {
                            KeyframeDecision::ForceNow => {
                                wctl.force_idr.store(true, Ordering::Relaxed);
                                cs.keyframes_forced += 1;
                            }
                            KeyframeDecision::Deferred => {}
                            KeyframeDecision::Ignored => {
                                cs.keyframe_requests_ignored += 1;
                                tracing::debug!(rtt = ?conn.rtt(), "RequestKeyframe ignorado (<1 RTT del último IDR)");
                            }
                        }
                    }
                    Ok(Some(ControlMessage::ReceiverStats(s))) => {
                        cs.receiver_stats += 1;
                        tracing::info!(?s, "ReceiverStats");
                        if let Some(afec) = adaptive_fec.as_mut() {
                            if let Some(ch) = afec.on_stats(s.frames_received, s.frames_lost, s.pre_fec_loss_bp) {
                                let _ = fec_tx.send(ch.to);
                                cs.fec_changes += 1;
                                cs.fec_final = ch.to;
                                cs.fec_min = cs.fec_min.min(ch.to);
                                cs.fec_max = cs.fec_max.max(ch.to);
                                tracing::info!(from = ch.from, to = ch.to, reason = ?ch.reason, "FEC adaptativo actualizado");
                            }
                        }
                        cs.last_receiver_stats = Some(s);
                    }
                    Ok(Some(ControlMessage::Bye { reason })) => {
                        peer_gone = true;
                        break 'session format!("el receptor se despidió ({reason:?})");
                    }
                    Ok(Some(ControlMessage::StopStream { .. })) => {
                        break 'session "el receptor pidió StopStream".into();
                    }
                    Ok(Some(ControlMessage::Input(ev))) => {
                        if cfg.accept_input && ev.stream_id() == STREAM_ID {
                            if let Some(rect) = *rect_rx.borrow() {
                                let target_rect = ScreenRect {
                                    x: rect.left,
                                    y: rect.top,
                                    width: rect.width(),
                                    height: rect.height(),
                                };
                                match injector.inject(&ev, target_rect) {
                                    Ok(()) => cs.input_events_injected += 1,
                                    Err(e) => {
                                        cs.input_events_blocked += 1;
                                        tracing::debug!("entrada remota no inyectada: {e}");
                                    }
                                }
                            }
                        }
                    }
                    Ok(Some(other)) => tracing::debug!(?other, "mensaje de control ignorado"),
                    Ok(None) => {
                        peer_gone = true;
                        break "el receptor cerró el canal de control".into();
                    }
                    Err(e) => {
                        peer_gone = true;
                        break format!("conexión perdida: {e}");
                    }
                },
                r = pos_rx.changed(), if pos_alive => {
                    if r.is_err() {
                        pos_alive = false;
                        continue;
                    }
                    let p = *pos_rx.borrow_and_update();
                    if let Some(p) = p {
                        let msg = ControlMessage::CursorPosition {
                            stream_id: STREAM_ID,
                            x: p.x,
                            y: p.y,
                            visible: p.visible,
                        };
                        if let Err(e) = ctl.send(&msg).await {
                            peer_gone = true;
                            break format!("conexión perdida: {e}");
                        }
                        cs.cursor_positions += 1;
                    }
                }
                r = shape_rx.changed(), if shape_alive => {
                    if r.is_err() {
                        shape_alive = false;
                        continue;
                    }
                    let s = shape_rx.borrow_and_update().clone();
                    if let Some(s) = s {
                        if let Err(e) = ctl.send(&ControlMessage::CursorShape((*s).clone())).await {
                            peer_gone = true;
                            break format!("conexión perdida: {e}");
                        }
                        cs.cursor_shapes += 1;
                    }
                }
            }
        };

        // Si se pierde la conexión, liberar al instante cualquier tecla o botón
        // que el receptor hubiera dejado pulsado antes del corte.
        let _ = injector.release_all();

        // Si no hay reconexión configurada (o grace == 0), terminar aquí.
        let Some(reconn) = cfg.reconnect.as_ref().filter(|r| !r.grace.is_zero()) else {
            break 'session drop_reason;
        };

        tracing::warn!(
            reason = %drop_reason,
            grace_s = reconn.grace.as_secs_f64(),
            "conexión interrumpida; intentando reconectar sin retirar el monitor virtual"
        );
        println!(
            "Conexión interrumpida ({drop_reason}); intentando reconectar durante {:.0} s…",
            reconn.grace.as_secs_f64()
        );
        wctl.streaming.store(false, Ordering::Relaxed);
        let _ = stop_video_tx.send(true);
        let (sstats, mut recovered_rx) =
            match tokio::time::timeout(Duration::from_secs(2), &mut video).await {
                Ok(Ok(pair)) => pair,
                _ => break 'session drop_reason,
            };
        conn.close(b"reconnecting");

        match try_reconnect(reconn, &cfg, display_mode, &mut shutdown, deadline).await {
            Ok(Some((new_conn, new_ctl))) => {
                conn = new_conn;
                ctl = new_ctl;
                peer_gone = false;
                while recovered_rx.try_recv().is_ok() {}
                wctl.force_idr.store(true, Ordering::Relaxed);
                wctl.streaming.store(true, Ordering::Relaxed);
                coalescer.note_forced(Instant::now());
                let (tx, rx) = watch::channel(false);
                stop_video_tx = tx;
                video = tokio::spawn(video_task(
                    conn.clone(),
                    recovered_rx,
                    fec_rx.clone(),
                    rx,
                    cfg.clone(),
                    frame_interval,
                    sstats,
                ));
                pos_rx.mark_changed();
                shape_rx.mark_changed();
                cs.reconnections += 1;
                println!(
                    "Reconectado con {} (reconexión #{})",
                    conn.remote_addr(),
                    cs.reconnections
                );
            }
            Ok(None) => {
                // Cancelado por shutdown o deadline durante la espera.
                video = tokio::spawn(async move { (sstats, recovered_rx) });
                if *shutdown.borrow() {
                    break 'session "parada pedida (Ctrl+C)".into();
                }
                break 'session "duración cumplida (--seconds)".into();
            }
            Err(e) => {
                tracing::warn!("no se pudo reconectar en la ventana de gracia: {e:#}");
                video = tokio::spawn(async move { (sstats, recovered_rx) });
                break 'session format!("{drop_reason} (reconexión agotada: {e})");
            }
        }
    };
    let duration = start.elapsed();
    println!("Fin de la sesión: {end_reason}");
    let _ = injector.release_all();

    // Cierre: Bye (si el par sigue), parar el hilo, vaciar el envío, retirar
    // el monitor.
    if !peer_gone {
        let reason = if end_reason.starts_with("fallo") {
            ByeReason::Error
        } else {
            ByeReason::UserRequested
        };
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            ctl.send(&ControlMessage::Bye { reason }),
        )
        .await;
        ctl.finish();
    }
    let _ = stop_video_tx.send(true);
    let wstats = worker.stop().await;
    let sstats = match tokio::time::timeout(Duration::from_secs(2), video).await {
        Ok(Ok((s, _))) => s,
        _ => SendStats::default(),
    };
    if let Some(g) = guard {
        match tokio::task::spawn_blocking(move || g.hide()).await {
            Ok(Ok(())) => println!("Monitor virtual retirado."),
            Ok(Err(e)) => eprintln!("AVISO: no se pudo retirar el monitor virtual: {e}"),
            Err(e) => eprintln!("AVISO: fallo al retirar el monitor virtual: {e}"),
        }
    }
    // Dar tiempo a que el Bye salga antes de cerrar la conexión.
    let _ = tokio::time::timeout(Duration::from_millis(300), conn.closed()).await;
    conn.close(b"bye");
    Ok(SenderReport {
        duration,
        end_reason,
        worker: wstats,
        send: sstats,
        control: cs,
    })
}

/// Escala la tasa CBR por defecto (20 Mbps para 1080p60) según los píxeles
/// reales del modo cuando es menor que 1080p, para que el codificador en modo
/// CBR no rellene cada P-frame a ~40 KiB ni cada IDR a ~100 KiB en pantallas
/// 1366×768 sobre Wi-Fi.
fn effective_bitrate_kbps(configured_kbps: u32, mode: Mode) -> u32 {
    const REF_PIXELS: u64 = 1920 * 1080;
    let pixels = u64::from(mode.width) * u64::from(mode.height);
    if configured_kbps == 20_000 && pixels > 0 && pixels < REF_PIXELS {
        ((16_000 * pixels / REF_PIXELS) as u32).clamp(4_000, configured_kbps)
    } else {
        configured_kbps
    }
}

async fn sleep_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Negocia el canal de control de una reconexión manteniendo el `display_mode`
/// ya activo en el monitor virtual y el hilo de captura.
async fn handshake_reconnect(
    conn: &Connection,
    cfg: &SenderConfig,
    display_mode: Mode,
) -> Result<FramedStream> {
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
    let peer = match recv_msg(&mut ctl, "Hello").await? {
        ControlMessage::Hello(h) => h,
        ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
        other => bail!("se esperaba Hello y llegó {other:?}"),
    };
    if !PROTOCOL_VERSION.is_compatible_with(&peer.version) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::IncompatibleVersion,
            })
            .await;
        ctl.finish();
        bail!(
            "versión de protocolo incompatible: local {:?}, receptor {:?}",
            PROTOCOL_VERSION,
            peer.version
        );
    }
    let sink: SinkCapabilities = loop {
        match recv_msg(&mut ctl, "SinkCapabilities").await? {
            ControlMessage::SinkCapabilities(c) => break c,
            ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
            other => tracing::debug!(?other, "mensaje ignorado antes de SinkCapabilities"),
        }
    };
    if !sink.decoders.iter().any(|d| d.codec == Codec::H264) {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::Error,
            })
            .await;
        bail!("el receptor no anuncia decodificador H.264");
    }
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: STREAM_ID,
        mode: cfg.mode,
        codec: Codec::H264,
        display_mode,
        bitrate_kbps: cfg.bitrate_kbps,
    }))
    .await?;
    let started = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            match ctl.expect::<ControlMessage>().await? {
                ControlMessage::StreamStarted { stream_id } if stream_id == STREAM_ID => {
                    return Ok(())
                }
                ControlMessage::StreamFailed { reason, .. } => {
                    bail!("el receptor no pudo iniciar el flujo: {reason}")
                }
                ControlMessage::Bye { reason } => bail!("el receptor cerró la sesión: {reason:?}"),
                other => tracing::debug!(?other, "mensaje ignorado antes de StreamStarted"),
            }
        }
    })
    .await;
    match started {
        Ok(Ok(())) => Ok(ctl),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("el receptor no confirmó StreamStarted en {START_TIMEOUT:?}"),
    }
}

/// Intenta reconectar al receptor durante `reconn.grace` con *backoff*
/// exponencial (250 ms → 2 s). Devuelve `Ok(None)` si el usuario pide parar o
/// se cumple `deadline` durante la espera.
async fn try_reconnect(
    reconn: &crate::ReconnectConfig,
    cfg: &SenderConfig,
    display_mode: Mode,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<(Connection, FramedStream)>> {
    let grace_deadline = tokio::time::Instant::now() + reconn.grace;
    let mut delay = Duration::from_millis(250);
    let mut last_err = anyhow::anyhow!("tiempo de gracia agotado");

    loop {
        let now = tokio::time::Instant::now();
        if now >= grace_deadline {
            return Err(last_err);
        }
        let rem = grace_deadline - now;
        let attempt_timeout = rem.min(Duration::from_secs(4));
        let attempt = async {
            let conn = reconn
                .endpoint
                .connect(reconn.addr, PeerPolicy::Exactly(reconn.peer), ALPN)
                .await
                .context("conectando QUIC")?;
            match handshake_reconnect(&conn, cfg, display_mode).await {
                Ok(ctl) => Ok((conn, ctl)),
                Err(e) => {
                    conn.close(b"handshake failed");
                    Err(e)
                }
            }
        };

        tokio::select! {
            r = shutdown.changed() => {
                if r.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
            _ = sleep_opt(deadline) => return Ok(None),
            res = tokio::time::timeout(attempt_timeout, attempt) => {
                match res {
                    Ok(Ok(pair)) => return Ok(Some(pair)),
                    Ok(Err(e)) => last_err = e,
                    Err(_) => last_err = anyhow::anyhow!("tiempo agotado en intento de reconexión"),
                }
            }
        }

        let now = tokio::time::Instant::now();
        if now >= grace_deadline {
            return Err(last_err);
        }
        let wait = delay.min(grace_deadline - now);
        tokio::select! {
            r = shutdown.changed() => {
                if r.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
            _ = sleep_opt(deadline) => return Ok(None),
            _ = tokio::time::sleep(wait) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

/// Fragmenta cada unidad, la reparte con el *pacer* y la envía.
async fn video_task(
    conn: Connection,
    mut rx: mpsc::Receiver<VideoUnit>,
    fec_rx: watch::Receiver<f32>,
    mut stop_rx: watch::Receiver<bool>,
    cfg: SenderConfig,
    interval: Duration,
    mut st: SendStats,
) -> (SendStats, mpsc::Receiver<VideoUnit>) {
    let Some(max_dg) = conn.max_datagram_size() else {
        tracing::error!("el receptor no admite datagramas QUIC");
        return (st, rx);
    };
    let mut packetizer = Packetizer::new(
        STREAM_ID,
        PacketizerConfig {
            max_datagram: max_dg,
            fec_ratio: *fec_rx.borrow(),
        },
    )
    .with_keyframe_fec_ratio(cfg.keyframe_fec_ratio)
    .with_interleave(cfg.fec_interleave);
    // Tope de ritmo para `Spread`: evita que un IDR con 50 % de FEC explote a
    // >100 Mbps en 10 ms y desborde la cola del Wi-Fi.
    let max_rate_bps = (f64::from(cfg.bitrate_kbps) * 1000.0 / 8.0 * 2.2).max(2_500_000.0);
    let mut pacer = Pacer::new(PacerConfig {
        mode: cfg.pacing,
        burst_bytes: 4 * max_dg,
    })
    .with_max_rate(max_rate_bps);
    let opts = PacedSendOptions {
        wait: cfg.pacing_wait,
        ..PacedSendOptions::default()
    };
    let mut sim_loss = cfg
        .sim_loss
        .map(|(model, seed)| LossInjector::new(model, seed));
    let mut seq: u32 = 0;
    loop {
        if *stop_rx.borrow() {
            break;
        }
        let u = tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
                continue;
            }
            u = rx.recv() => match u {
                Some(u) => u,
                None => break,
            },
        };
        if let Some(m) = conn.max_datagram_size() {
            packetizer.set_max_datagram(m);
        }
        packetizer.set_fec_ratio(*fec_rx.borrow());
        let meta = FrameMeta {
            frame_seq: seq,
            pts_us: wire_pts(u.pts_us),
            keyframe: u.keyframe,
            discardable: false,
        };
        let dgs = match packetizer.packetize(meta, &u.data) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("no se pudo fragmentar el frame {seq}: {e}");
                continue;
            }
        };
        seq = seq.wrapping_add(1);
        let sizes: Vec<usize> = dgs.iter().map(Vec::len).collect();
        st.wire_bytes += sizes.iter().sum::<usize>() as u64;
        let schedule = pacer.schedule(&sizes, interval, Instant::now());
        let mut drops = (0..dgs.len())
            .map(|_| sim_loss.as_mut().is_some_and(LossInjector::drop_next))
            .collect::<Vec<_>>()
            .into_iter();
        let c = conn.clone();
        let r = pacer::send_paced(
            dgs.into_iter().map(Bytes::from).collect(),
            schedule,
            opts,
            move |d| {
                if drops.next().unwrap_or(false) {
                    return true;
                }
                c.send_datagram(d).is_ok()
            },
        )
        .await;
        st.frames_sent += 1;
        st.keyframes_sent += u64::from(u.keyframe);
        st.payload_bytes += u.data.len() as u64;
        st.datagrams_sent += r.sent as u64;
        st.datagrams_failed += r.failed as u64;
        st.max_pacing_late = st.max_pacing_late.max(r.max_late);
        st.capture_to_sent_us
            .push(qpc_now_us().saturating_sub(u.pts_us) as f64);
    }
    if let Some(inj) = sim_loss {
        st.datagrams_sent = st.datagrams_sent.saturating_sub(inj.dropped);
        st.sim_dropped_datagrams += inj.dropped;
        st.sim_bursts += inj.bursts;
        st.sim_longest_burst = st.sim_longest_burst.max(inj.longest_burst);
    }
    (st, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReconnectConfig;
    use cs_core::CodecSupport;
    use cs_security::identity::Identity;
    use cs_transport::Endpoint;

    #[test]
    fn effective_bitrate_scales_below_1080p_default() {
        let m768 = Mode::new(1366, 768, 60);
        assert_eq!(effective_bitrate_kbps(20_000, m768), 8094);
        let m1080 = Mode::new(1920, 1080, 60);
        assert_eq!(effective_bitrate_kbps(20_000, m1080), 20_000);
        assert_eq!(effective_bitrate_kbps(12_000, m768), 12_000);
    }

    #[tokio::test]
    async fn try_reconnect_recovers_within_grace_window() {
        let src_id = Identity::generate().unwrap();
        let sink_id = Identity::generate().unwrap();
        let src_fp = src_id.fingerprint();
        let sink_fp = sink_id.fingerprint();

        let server = Endpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            sink_id,
            PeerPolicy::Exactly(src_fp),
            ALPN,
        )
        .unwrap();
        let addr = server.local_addr().unwrap();

        let srv = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().unwrap();
            let mut ctl = FramedStream::new(conn.accept_bi().await.unwrap());
            let _hello: ControlMessage = ctl.expect().await.unwrap();
            ctl.send(&ControlMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                device_id: sink_fp.device_id(),
                device_name: "sink-test".into(),
                roles: Roles {
                    source: false,
                    sink: true,
                },
            }))
            .await
            .unwrap();
            ctl.send(&ControlMessage::SinkCapabilities(SinkCapabilities {
                native_mode: Mode::new(1366, 768, 60),
                dpi: 96,
                decoders: vec![CodecSupport {
                    codec: Codec::H264,
                    max_h264_profile: None,
                    hardware: true,
                    max_pixel_rate: 0,
                }],
            }))
            .await
            .unwrap();
            let msg: ControlMessage = ctl.expect().await.unwrap();
            match msg {
                ControlMessage::StartStream(s) => {
                    assert_eq!(s.display_mode, Mode::new(1366, 768, 60));
                    ctl.send(&ControlMessage::StreamStarted {
                        stream_id: s.stream_id,
                    })
                    .await
                    .unwrap();
                }
                other => panic!("esperaba StartStream, llegó {other:?}"),
            }
            conn
        });

        let client = Endpoint::client("127.0.0.1:0".parse().unwrap(), src_id).unwrap();
        let reconn = ReconnectConfig {
            endpoint: client.clone(),
            addr,
            peer: sink_fp,
            grace: Duration::from_secs(5),
        };
        let cfg = SenderConfig::new(src_fp.device_id(), "src-test".into());
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        let res = try_reconnect(&reconn, &cfg, Mode::new(1366, 768, 60), &mut stop_rx, None)
            .await
            .unwrap();
        assert!(res.is_some());
        let (conn, _ctl) = res.unwrap();
        let srv_conn = srv.await.unwrap();
        conn.close(b"done");
        srv_conn.close(b"done");
        client.close();
    }

    #[tokio::test]
    async fn try_reconnect_aborts_on_shutdown() {
        let src_id = Identity::generate().unwrap();
        let sink_id = Identity::generate().unwrap();
        let src_fp = src_id.fingerprint();
        let client = Endpoint::client("127.0.0.1:0".parse().unwrap(), src_id).unwrap();
        let reconn = ReconnectConfig {
            endpoint: client.clone(),
            // Puerto cerrado local para que falle o espere y cancelamos por shutdown.
            addr: "127.0.0.1:9".parse().unwrap(),
            peer: sink_id.fingerprint(),
            grace: Duration::from_secs(10),
        };
        let cfg = SenderConfig::new(src_fp.device_id(), "src-test".into());
        let (stop_tx, mut stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = stop_tx.send(true);
        });
        let res = try_reconnect(&reconn, &cfg, Mode::new(1366, 768, 60), &mut stop_rx, None)
            .await
            .unwrap();
        assert!(res.is_none());
        client.close();
    }
}
