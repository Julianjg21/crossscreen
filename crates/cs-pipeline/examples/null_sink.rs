//! Receptor "nulo" de pruebas del emisor (desechable; NO es el visor).
//!
//! Habla el contrato `docs/09-contrato-sesion-mvp.md` como receptor:
//! escucha, acepta solo pares emparejados, responde Hello +
//! SinkCapabilities + StreamStarted, reensambla los datagramas y **valida**
//! cada unidad H.264 (Annex-B, NAL, SPS/PPS en cada IDR, primer frame IDR),
//! sin decodificar. Mide, con el mismo QPC que el emisor (mismo equipo), la
//! latencia captura → frame reensamblado.
//!
//! ```text
//! cargo run -p cs-pipeline --example null_sink --release -- \
//!     --data-dir D --listen 127.0.0.1:47810 [--seconds 20] [--drop-every 300] \
//!     [--request-keyframe-every 3] [--dump salida.h264]
//! ```

use anyhow::{bail, Context, Result};
use clap::Parser;
use cs_capture::stats::Samples;
use cs_core::{Codec, CodecSupport, H264Profile, Mode, Roles};
use cs_encode::annexb::{self, nal_type};
use cs_encode::h264::parse_sps;
use cs_protocol::control::{ByeReason, ControlMessage, Hello, ReceiverStats, SinkCapabilities};
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::trust::TrustStore;
use cs_security::verifier::PeerPolicy;
use cs_transport::{Endpoint, FramedStream, Reassembler, ReassemblyEvent};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long, default_value = "127.0.0.1:47810")]
    listen: SocketAddr,
    /// Modo nativo anunciado (ancho, alto, Hz).
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    hz: u32,
    /// Despedirse (Bye) tras S segundos de flujo; sin esto, hasta que el emisor cierre.
    #[arg(long)]
    seconds: Option<u64>,
    /// Simular pérdida: ignorar los datagramas de 1 de cada N frames
    /// (provoca `Lost` → `RequestKeyframe`). 0 = nunca.
    #[arg(long, default_value_t = 0)]
    drop_every: u32,
    /// Pedir un IDR cada S segundos aunque no haya pérdidas. 0 = nunca.
    #[arg(long, default_value_t = 0.0)]
    request_keyframe_every: f64,
    /// Guardar el flujo H.264 recibido (Annex-B).
    #[arg(long)]
    dump: Option<PathBuf>,
}

#[derive(Default)]
struct Check {
    frames: u64,
    keyframes: u64,
    bytes: u64,
    lost: u64,
    gaps: u64,
    bad_start_code: u64,
    forbidden_bit: u64,
    idr_without_ps: u64,
    flag_mismatch: u64,
    first_not_idr: bool,
    nal_hist: [u64; 32],
    sps: Option<String>,
    latency_us: Samples,
    keyframe_response_ms: Samples,
    cursor_positions: u64,
    cursor_shapes: u64,
    last_cursor: Option<(i32, i32, bool)>,
    last_shape: Option<(u16, u16, u16, u16)>,
    waiting_idr_since: Option<Instant>,
    requests: u64,
}

#[cfg(windows)]
fn qpc_us() -> u64 {
    cs_capture::windows_dxgi::qpc_now_us()
}

/// Sin QPC fuera de Windows: la latencia saldrá fuera de rango y no se cuenta.
#[cfg(not(windows))]
fn qpc_us() -> u64 {
    0
}

impl Check {
    fn frame(&mut self, f: &cs_transport::CompleteFrame) {
        let lat = (qpc_us() as u32).wrapping_sub(f.pts_us);
        // Más de 10 s indica relojes distintos o pts no QPC: no se cuenta.
        if lat < 10_000_000 {
            self.latency_us.push(lat as f64);
        }
        if self.frames == 0 && !f.keyframe {
            self.first_not_idr = true;
        }
        self.frames += 1;
        self.bytes += f.data.len() as u64;
        if !annexb::starts_with_start_code(&f.data) {
            self.bad_start_code += 1;
        }
        let s = annexb::summarize(&f.data);
        for t in &s.nal_types {
            self.nal_hist[usize::from(*t & 31)] += 1;
        }
        if s.forbidden_bit_set {
            self.forbidden_bit += 1;
        }
        if f.keyframe {
            self.keyframes += 1;
            if let Some(t) = self.waiting_idr_since.take() {
                self.keyframe_response_ms
                    .push(t.elapsed().as_secs_f64() * 1000.0);
            }
            if !(s.has_sps && s.has_pps && s.has_idr) {
                self.idr_without_ps += 1;
            }
            if self.sps.is_none() {
                if let Some(n) = annexb::split_nals(&f.data)
                    .into_iter()
                    .find(|n| n.nal_type() == nal_type::SPS)
                {
                    self.sps = Some(match parse_sps(n.data) {
                        Ok(p) => format!(
                            "profile {} level {} {}x{} refs {} reorder {:?}",
                            p.profile_idc,
                            p.level_idc,
                            p.width,
                            p.height,
                            p.max_num_ref_frames,
                            p.vui.as_ref().and_then(|v| v.max_num_reorder_frames)
                        ),
                        Err(e) => format!("SPS ilegible: {e:?}"),
                    });
                }
            }
        }
        if f.keyframe != s.has_idr {
            self.flag_mismatch += 1;
        }
    }
}

fn pct(s: &mut Samples, scale: f64) -> String {
    match (
        s.percentile(50.0),
        s.percentile(95.0),
        s.percentile(99.0),
        s.max(),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => format!(
            "p50 {:.2} / p95 {:.2} / p99 {:.2} / máx {:.2} ms (n={})",
            a * scale,
            b * scale,
            c * scale,
            d * scale,
            s.len()
        ),
        _ => "sin muestras".into(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let a = Args::parse();
    let identity = Identity::load_or_create(&a.data_dir)?;
    let trust = TrustStore::open(a.data_dir.join("trusted-peers.json"))?;
    let fps = trust.fingerprints();
    if fps.is_empty() {
        bail!("no hay pares emparejados en {}", a.data_dir.display());
    }
    let endpoint = Endpoint::server(a.listen, identity.clone(), PeerPolicy::pinned(fps), ALPN)?;
    println!("null_sink escuchando en {}", endpoint.local_addr()?);
    let conn = endpoint
        .accept()
        .await
        .context("endpoint cerrado")?
        .context("handshake")?;
    println!(
        "Conexión de {} ({})",
        conn.remote_addr(),
        conn.peer_fingerprint().short()
    );
    let mut ctl = FramedStream::new(conn.accept_bi().await?);

    match ctl.expect::<ControlMessage>().await? {
        ControlMessage::Hello(h) => println!("Hello de '{}' v{:?}", h.device_name, h.version),
        other => bail!("se esperaba Hello: {other:?}"),
    }
    ctl.send(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        device_id: identity.fingerprint().device_id(),
        device_name: "null_sink".into(),
        roles: Roles {
            source: false,
            sink: true,
        },
    }))
    .await?;
    ctl.send(&ControlMessage::SinkCapabilities(SinkCapabilities {
        decoders: vec![CodecSupport {
            codec: Codec::H264,
            max_h264_profile: Some(H264Profile::High),
            hardware: true,
            max_pixel_rate: 3840 * 2160 * 60,
        }],
        native_mode: Mode::new(a.width, a.height, a.hz),
        dpi: 96,
    }))
    .await?;
    let start = match ctl.expect::<ControlMessage>().await? {
        ControlMessage::StartStream(s) => s,
        other => bail!("se esperaba StartStream: {other:?}"),
    };
    println!("StartStream: {start:?}");
    ctl.send(&ControlMessage::StreamStarted {
        stream_id: start.stream_id,
    })
    .await?;

    let mut dump = match &a.dump {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => None,
    };
    let mut r = Reassembler::new(start.stream_id);
    let mut c = Check::default();
    let t0 = Instant::now();
    let end = a
        .seconds
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    let kf_every =
        (a.request_keyframe_every > 0.0).then(|| Duration::from_secs_f64(a.request_keyframe_every));
    let mut next_kf = kf_every.map(|d| Instant::now() + d);
    let mut dgram_frame_seen = std::collections::HashSet::new();
    let mut i_sent_bye = false;
    let mut last_seq: Option<u32> = None;
    let reason = loop {
        tokio::select! {
            _ = async { match end { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                ctl.send(&ControlMessage::Bye { reason: ByeReason::UserRequested }).await?;
                i_sent_bye = true;
                break "duración cumplida".to_string();
            }
            _ = stats_tick.tick() => {
                let s = r.stats();
                ctl.send(&ControlMessage::ReceiverStats(ReceiverStats {
                    stream_id: start.stream_id,
                    frames_received: s.frames_delivered as u32,
                    frames_lost: s.frames_lost as u32,
                    pre_fec_loss_bp: 0,
                    jitter_us: 0,
                    decode_time_us: 0,
                })).await?;
                if let (Some(t), Some(d)) = (next_kf, kf_every) {
                    if Instant::now() >= t {
                        ctl.send(&ControlMessage::RequestKeyframe { stream_id: start.stream_id }).await?;
                        c.requests += 1;
                        c.waiting_idr_since.get_or_insert(Instant::now());
                        next_kf = Some(Instant::now() + d);
                    }
                }
            }
            d = conn.read_datagram() => {
                let Ok(d) = d else { break "conexión cerrada".to_string() };
                // Simulación de pérdida: descartar todos los fragmentos de 1 de cada N frames.
                if a.drop_every > 0 && d.len() >= 8 {
                    if let Ok((h, _)) = cs_protocol::video::FragmentHeader::decode(&d) {
                        if h.frame_seq > 0 && h.frame_seq % a.drop_every == 0 {
                            dgram_frame_seen.insert(h.frame_seq);
                            continue;
                        }
                    }
                }
                let Ok(evs) = r.push(&d) else { continue };
                for ev in evs {
                    match ev {
                        ReassemblyEvent::Frame(f) => {
                            // Un frame del que no llegó NINGÚN fragmento no genera
                            // `Lost` en el Reassembler: detectar el hueco en frame_seq.
                            let gap = last_seq.is_some_and(|l: u32| f.frame_seq != l.wrapping_add(1));
                            last_seq = Some(f.frame_seq);
                            if gap {
                                c.gaps += 1;
                                ctl.send(&ControlMessage::RequestKeyframe { stream_id: start.stream_id }).await?;
                                c.requests += 1;
                                c.waiting_idr_since.get_or_insert(Instant::now());
                            }
                            c.frame(&f);
                            if let Some(w) = dump.as_mut() { w.write_all(&f.data)?; }
                        }
                        ReassemblyEvent::Lost { frame_seq } => {
                            c.lost += 1;
                            tracing::info!(frame_seq, "frame perdido → RequestKeyframe");
                            ctl.send(&ControlMessage::RequestKeyframe { stream_id: start.stream_id }).await?;
                            c.requests += 1;
                            c.waiting_idr_since.get_or_insert(Instant::now());
                        }
                    }
                }
            }
            m = ctl.recv::<ControlMessage>() => match m {
                Ok(Some(ControlMessage::CursorPosition { x, y, visible, .. })) => {
                    c.cursor_positions += 1;
                    c.last_cursor = Some((x, y, visible));
                }
                Ok(Some(ControlMessage::CursorShape(s))) => {
                    c.cursor_shapes += 1;
                    if !s.validate() { println!("AVISO: CursorShape inválido"); }
                    c.last_shape = Some((s.width, s.height, s.hotspot_x, s.hotspot_y));
                }
                Ok(Some(ControlMessage::Bye { reason })) => break format!("Bye del emisor ({reason:?})"),
                Ok(Some(other)) => println!("control: {other:?}"),
                Ok(None) => break "canal de control cerrado".to_string(),
                Err(e) => break format!("error de control: {e}"),
            }
        }
    };
    if !i_sent_bye {
        let _ = ctl
            .send(&ControlMessage::Bye {
                reason: ByeReason::Shutdown,
            })
            .await;
    }
    ctl.finish();
    if let Some(mut w) = dump {
        w.flush()?;
    }
    let secs = t0.elapsed().as_secs_f64();
    let s = r.stats();
    println!("── null_sink: resumen ({secs:.1} s, fin: {reason}) ──");
    println!(
        "Frames {} ({:.1} fps) · IDR {} · perdidos {} + huecos en frame_seq {} (simulados {}) · {:.2} Mbps",
        c.frames,
        c.frames as f64 / secs,
        c.keyframes,
        c.lost,
        c.gaps,
        dgram_frame_seen.len(),
        c.bytes as f64 * 8.0 / secs / 1e6
    );
    println!("Reensamblado: {s:?}");
    println!(
        "Validación: primer frame no IDR={} · sin start code {} · forbidden_bit {} · IDR sin SPS/PPS {} · flag IDR ≠ NAL {}",
        c.first_not_idr, c.bad_start_code, c.forbidden_bit, c.idr_without_ps, c.flag_mismatch
    );
    let hist: Vec<String> = c
        .nal_hist
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(t, n)| format!("{t}:{n}"))
        .collect();
    println!("NAL por tipo: {}", hist.join(" "));
    println!("SPS: {}", c.sps.as_deref().unwrap_or("-"));
    println!(
        "Latencia captura→reensamblado (mismo QPC): {}",
        pct(&mut c.latency_us, 1e-3)
    );
    println!(
        "RequestKeyframe enviados {} · respuesta (petición→IDR recibido): {}",
        c.requests,
        pct(&mut c.keyframe_response_ms, 1.0)
    );
    println!(
        "Cursor: {} posiciones (última {:?}) · {} formas (última {:?})",
        c.cursor_positions, c.last_cursor, c.cursor_shapes, c.last_shape
    );
    let _ = tokio::time::timeout(Duration::from_secs(1), conn.closed()).await;
    endpoint.close();
    let ok = c.frames > 0
        && !c.first_not_idr
        && c.bad_start_code == 0
        && c.forbidden_bit == 0
        && c.idr_without_ps == 0;
    println!(
        "{}",
        if ok {
            "VALIDACIÓN OK"
        } else {
            "VALIDACIÓN FALLIDA"
        }
    );
    Ok(())
}
