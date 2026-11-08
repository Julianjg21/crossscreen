//! Sesión emisora: canal de control (contrato §"Canal de control"), monitor
//! virtual, hilo de captura + codificación y tarea de envío de vídeo.

use crate::display_guard::{mode_candidates, VirtualDisplayGuard};
use crate::stats::{ControlStats, SendStats, SenderReport};
use crate::timing::{wire_pts, KeyframeCoalescer};
use crate::win::worker::{self, CursorPos, VideoUnit, WorkerControl, WorkerOutputs, WorkerParams};
use crate::{SenderConfig, STREAM_ID};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use cs_capture::windows_dxgi::{list_outputs, qpc_now_us};
use cs_capture::CaptureTarget;
use cs_core::{Codec, Mode, Roles, StreamMode};
use cs_protocol::control::{
    ByeReason, ControlMessage, CursorShape, Hello, SinkCapabilities, StartStream,
};
use cs_protocol::PROTOCOL_VERSION;
use cs_transport::pacer::{self, PacedSendOptions, Pacer, PacerConfig};
use cs_transport::packetizer::FrameMeta;
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
    conn: Connection,
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

    // Hilo de captura + codificación (arranca ya; codifica tras StreamStarted).
    let (video_tx, video_rx) = mpsc::channel::<VideoUnit>(1);
    let (pos_tx, mut pos_rx) = watch::channel::<Option<CursorPos>>(None);
    let (shape_tx, mut shape_rx) = watch::channel::<Option<Arc<CursorShape>>>(None);
    let wctl = Arc::new(WorkerControl::default());
    let (join, ready, mut worker_done) = worker::spawn(
        WorkerParams {
            target,
            fps,
            bitrate_kbps: cfg.bitrate_kbps,
            stream_id: STREAM_ID,
            static_refresh: cfg.static_refresh,
        },
        WorkerOutputs {
            video: video_tx,
            cursor_pos: pos_tx,
            cursor_shape: shape_tx,
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
        "Flujo iniciado: {}x{}@{fps} · {} kbps · FEC {} · pacing {:?}",
        display_mode.width, display_mode.height, cfg.bitrate_kbps, cfg.fec_ratio, cfg.pacing
    );
    // 6. El primer frame tras StreamStarted es IDR (el hilo arranca con need_idr).
    wctl.force_idr.store(true, Ordering::Relaxed);
    wctl.streaming.store(true, Ordering::Relaxed);
    let start = Instant::now();

    let video = tokio::spawn(video_task(
        conn.clone(),
        video_rx,
        cfg.clone(),
        Duration::from_micros(1_000_000 / u64::from(fps)),
    ));

    // Bucle de control.
    let mut cs = ControlStats::default();
    let mut coalescer = KeyframeCoalescer::new(KEYFRAME_MIN_INTERVAL);
    let deadline = cfg.duration.map(|d| tokio::time::Instant::now() + d);
    let mut shutdown = shutdown;
    // Enviar el estado inicial del cursor si ya lo hay.
    pos_rx.mark_changed();
    shape_rx.mark_changed();
    let (mut pos_alive, mut shape_alive) = (true, true);
    let mut peer_gone = false;
    let end_reason: String = loop {
        let deferred = coalescer
            .deferred_until()
            .map(tokio::time::Instant::from_std);
        tokio::select! {
            r = shutdown.changed() => {
                if r.is_err() || *shutdown.borrow() {
                    break "parada pedida (Ctrl+C)".into();
                }
            }
            _ = sleep_opt(deadline) => break "duración cumplida (--seconds)".into(),
            _ = sleep_opt(deferred) => {
                if coalescer.poll(Instant::now()) {
                    wctl.force_idr.store(true, Ordering::Relaxed);
                    cs.keyframes_forced += 1;
                }
            }
            _ = &mut worker_done => break "fallo en captura/codificación".into(),
            m = ctl.recv::<ControlMessage>() => match m {
                Ok(Some(ControlMessage::RequestKeyframe { .. })) => {
                    cs.keyframe_requests += 1;
                    if coalescer.request(Instant::now()) {
                        wctl.force_idr.store(true, Ordering::Relaxed);
                        cs.keyframes_forced += 1;
                    }
                }
                Ok(Some(ControlMessage::ReceiverStats(s))) => {
                    cs.receiver_stats += 1;
                    tracing::info!(?s, "ReceiverStats");
                    cs.last_receiver_stats = Some(s);
                }
                Ok(Some(ControlMessage::Bye { reason })) => {
                    peer_gone = true;
                    break format!("el receptor se despidió ({reason:?})");
                }
                Ok(Some(ControlMessage::StopStream { .. })) => {
                    break "el receptor pidió StopStream".into();
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
    let duration = start.elapsed();
    println!("Fin de la sesión: {end_reason}");

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
    let wstats = worker.stop().await;
    let sstats = match tokio::time::timeout(Duration::from_secs(2), video).await {
        Ok(Ok(s)) => s,
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

async fn sleep_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Fragmenta cada unidad, la reparte con el *pacer* y la envía.
async fn video_task(
    conn: Connection,
    mut rx: mpsc::Receiver<VideoUnit>,
    cfg: SenderConfig,
    interval: Duration,
) -> SendStats {
    let mut st = SendStats::default();
    let Some(max_dg) = conn.max_datagram_size() else {
        tracing::error!("el receptor no admite datagramas QUIC");
        return st;
    };
    let mut packetizer = Packetizer::new(
        STREAM_ID,
        PacketizerConfig {
            max_datagram: max_dg,
            fec_ratio: cfg.fec_ratio,
        },
    );
    let mut pacer = Pacer::new(PacerConfig {
        mode: cfg.pacing,
        burst_bytes: 4 * max_dg,
    });
    let opts = PacedSendOptions {
        wait: cfg.pacing_wait,
        ..PacedSendOptions::default()
    };
    let mut seq: u32 = 0;
    while let Some(u) = rx.recv().await {
        if let Some(m) = conn.max_datagram_size() {
            packetizer.set_max_datagram(m);
        }
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
        let c = conn.clone();
        let r = pacer::send_paced(
            dgs.into_iter().map(Bytes::from).collect(),
            schedule,
            opts,
            move |d| c.send_datagram(d).is_ok(),
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
    st
}
