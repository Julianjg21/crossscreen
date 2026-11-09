//! Emisor Linux: XDG Desktop Portal → PipeWire → OpenH264 → QUIC.
//!
//! Esta primera implementación duplica el monitor seleccionado por Portal.
//! No crea una pantalla virtual Linux, por lo que `extend` se rechaza
//! explícitamente.

use crate::stats::{SenderReport, WorkerStats};
use crate::timing::wire_pts;
use crate::{SenderConfig, STREAM_ID};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use cs_capture::linux_pipewire::I420Buffer;
use cs_capture::linux_portal::PortalCaptureSession;
use cs_core::{Codec, Mode, Roles, StreamMode};
use cs_encode::openh264_backend::{I420Frame, OpenH264Encoder};
use cs_encode::{EncodeControl, EncoderConfig, VideoEncoder};
use cs_protocol::control::{ByeReason, ControlMessage, Hello, SinkCapabilities, StartStream};
use cs_protocol::PROTOCOL_VERSION;
use cs_transport::pacer::{self, PacedSendOptions, Pacer, PacerConfig};
use cs_transport::packetizer::FrameMeta;
use cs_transport::{Connection, FramedStream, Packetizer, PacketizerConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc as tokio_mpsc, watch};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(15);

struct LinuxUnit {
    data: Vec<u8>,
    keyframe: bool,
    pts_us: u64,
}

struct LinuxWorker {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<WorkerStats>>,
    capture_control: Option<cs_capture::linux_pipewire::PipeWireCaptureControl>,
}

impl LinuxWorker {
    fn start(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        cfg: &SenderConfig,
        tx: tokio_mpsc::Sender<LinuxUnit>,
    ) -> Result<Self> {
        let capture = cs_capture::linux_pipewire::start_i420(node_id, fd)
            .context("iniciando el stream PipeWire")?;
        let (frames, capture_control) = capture.into_parts();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let bitrate = cfg.bitrate_kbps;
        let join = std::thread::Builder::new()
            .name("crossscreen-linux-capture-encode".into())
            .spawn(move || encode_loop(frames, stop_thread, bitrate, tx))
            .context("creando el hilo Linux de captura")?;
        Ok(Self {
            stop,
            join: Some(join),
            capture_control: Some(capture_control),
        })
    }

    async fn stop(mut self) -> WorkerStats {
        self.stop.store(true, Ordering::Relaxed);
        let worker = self.join.take().map(|join| {
            tokio::task::spawn_blocking(move || {
                join.join().unwrap_or_else(|_| WorkerStats {
                    fatal: Some("el hilo Linux de captura terminó con pánico".into()),
                    ..WorkerStats::default()
                })
            })
        });
        let stats = worker.map(|task| task).map(|task| async move {
            task.await.unwrap_or_else(|_| WorkerStats {
                fatal: Some("no se pudo esperar al hilo Linux de captura".into()),
                ..WorkerStats::default()
            })
        });
        let result = match stats {
            Some(task) => task.await,
            None => WorkerStats::default(),
        };
        if let Some(control) = self.capture_control.take() {
            control.stop();
        }
        result
    }
}

fn encode_loop(
    frames: mpsc::Receiver<I420Buffer>,
    stop: Arc<AtomicBool>,
    bitrate_kbps: u32,
    tx: tokio_mpsc::Sender<LinuxUnit>,
) -> WorkerStats {
    let mut stats = WorkerStats::default();
    let mut encoder: Option<(OpenH264Encoder, (u32, u32))> = None;
    let mut force_idr = true;
    let mut next_pts_us = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let Ok(buffer) = frames.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        stats.captured += 1;
        stats.last_size = Some((buffer.width, buffer.height));
        if encoder
            .as_ref()
            .is_none_or(|(_, size)| *size != (buffer.width, buffer.height))
        {
            let config = EncoderConfig {
                bitrate_kbps,
                fps_num: 60,
                fps_den: 1,
                ..EncoderConfig::default()
            };
            match OpenH264Encoder::from_default_path(&config) {
                Ok(enc) => {
                    encoder = Some((enc, (buffer.width, buffer.height)));
                    stats.encoders_created += 1;
                    force_idr = true;
                }
                Err(error) => {
                    stats.fatal = Some(error.to_string());
                    break;
                }
            }
        }
        let frame = match I420Frame::new(
            buffer.width,
            buffer.height,
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
        let Some((encoder, _)) = encoder.as_mut() else {
            continue;
        };
        let units = match encoder.encode(&frame, EncodeControl { force_idr }) {
            Ok(units) => units,
            Err(error) => {
                stats.encoder_errors += 1;
                tracing::warn!("fallo codificando OpenH264: {error}");
                continue;
            }
        };
        force_idr = false;
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
                pts_us: next_pts_us,
            };
            next_pts_us = next_pts_us.saturating_add(16_667);
            match tx.try_send(item) {
                Ok(()) => {}
                Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                    stats.dropped_backpressure += 1;
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => return stats,
            }
        }
    }
    stats
}

pub(crate) async fn run(
    conn: Connection,
    cfg: SenderConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<SenderReport> {
    if cfg.mode != StreamMode::Duplicate {
        bail!("Linux solo admite el modo duplicate; extend requiere una pantalla virtual Linux");
    }
    if cfg.accept_input {
        tracing::info!("la entrada remota está desactivada en el emisor Linux experimental");
    }
    if cfg.clipboard {
        tracing::info!("el portapapeles está desactivado en el emisor Linux experimental");
    }
    let started_at = Instant::now();
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
    let portal = PortalCaptureSession::open()
        .await
        .context("autorizando captura mediante XDG Desktop Portal")?;
    let mode = portal
        .stream
        .size
        .map(|(width, height)| Mode::new(width.max(1) as u32, height.max(1) as u32, 60))
        .unwrap_or(sink.native_mode);
    let (video_tx, video_rx) = tokio_mpsc::channel(2);
    let worker = LinuxWorker::start(portal.stream.node_id, portal.pipewire_fd, &cfg, video_tx)?;
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: STREAM_ID,
        mode: StreamMode::Duplicate,
        codec: Codec::H264,
        display_mode: mode,
        bitrate_kbps: cfg.bitrate_kbps,
    }))
    .await?;
    expect_started(&mut ctl).await?;
    let send_task = tokio::spawn(send_video(
        conn.clone(),
        video_rx,
        cfg.clone(),
        Duration::from_micros(16_667),
    ));
    let end_reason = tokio::select! {
        result = shutdown.changed() => {
            if result.is_err() || *shutdown.borrow() {
                "parada pedida (Ctrl+C)".to_owned()
            } else {
                "canal de parada cerrado".to_owned()
            }
        }
        result = ctl.expect::<ControlMessage>() => {
            match result {
                Ok(ControlMessage::Bye { reason }) => format!("receptor cerró la sesión: {reason:?}"),
                Ok(other) => format!("control inesperado: {other:?}"),
                Err(error) => format!("conexión de control perdida: {error}"),
            }
        }
        _ = async {
            if let Some(duration) = cfg.duration {
                tokio::time::sleep(duration).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => "duración cumplida (--seconds)".to_owned(),
    };
    let _ = ctl
        .send(&ControlMessage::Bye {
            reason: ByeReason::UserRequested,
        })
        .await;
    ctl.finish();
    let worker_stats = worker.stop().await;
    let send_stats = send_task.await.unwrap_or_default();
    conn.close(b"bye");
    Ok(SenderReport {
        duration: started_at.elapsed(),
        end_reason,
        worker: worker_stats,
        send: send_stats,
        control: Default::default(),
    })
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

async fn send_video(
    conn: Connection,
    mut rx: tokio_mpsc::Receiver<LinuxUnit>,
    cfg: SenderConfig,
    interval: Duration,
) -> crate::stats::SendStats {
    let Some(max_datagram) = conn.max_datagram_size() else {
        return Default::default();
    };
    let mut stats = crate::stats::SendStats::default();
    let mut packetizer = Packetizer::new(
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
    while let Some(unit) = rx.recv().await {
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
    stats
}
