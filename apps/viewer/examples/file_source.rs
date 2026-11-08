//! Emisor de prueba **desechable** para probar `crossscreen-viewer --listen`
//! sin el emisor real: habla el contrato de `docs/09-contrato-sesion-mvp.md`
//! y transmite las AU de un fichero H.264 Annex-B con `pts_us` = QPC actual,
//! más mensajes de cursor sintéticos (forma y posición en círculo).
//!
//! Para probar la recuperación se pueden inducir pérdidas: `--loss P`
//! descarta cada datagrama con probabilidad P; `--drop-every N` descarta
//! todos los datagramas de uno de cada N frames. Ante `RequestKeyframe` el
//! «codificador» simulado salta al IDR más reciente del fichero (equivale a
//! forzar un IDR: es un punto de acceso aleatorio limpio).
//!
//! ```text
//! cargo run -p crossscreen-viewer --example file_source -- 127.0.0.1:47810 \
//!     --data-dir B --peer <prefijo> --file bench_1080p.h264 --seconds 10 [--drop-every 90]
//! ```
#![allow(unsafe_code)] // FFI QueryPerformanceCounter (mismo reloj que el visor).

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use clap::Parser;
use cs_core::{Codec, Mode, Roles, StreamMode};
use cs_decode::annexb::{is_idr, split_access_units};
use cs_protocol::control::{ByeReason, ControlMessage, CursorShape, Hello, StartStream};
use cs_protocol::{ALPN, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::trust::TrustStore;
use cs_security::verifier::PeerPolicy;
use cs_transport::pacer::send_paced;
use cs_transport::packetizer::FrameMeta;
use cs_transport::{
    Endpoint, FramedStream, PacedSendOptions, Pacer, PacerConfig, PacingMode, Packetizer,
    PacketizerConfig,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
struct Args {
    /// Dirección del visor (`crossscreen-viewer --listen`).
    addr: SocketAddr,
    /// Huella (o prefijo hexadecimal) del visor emparejado.
    #[arg(long)]
    peer: String,
    #[arg(long)]
    data_dir: PathBuf,
    /// Fichero H.264 Annex-B (se repite en bucle).
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,
    /// Probabilidad de descartar cada datagrama (0–1).
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Descartar todos los datagramas de uno de cada N frames (0 = nunca).
    #[arg(long, default_value_t = 0)]
    drop_every: u32,
    #[arg(long, default_value_t = 0.1)]
    fec: f32,
    #[arg(long, default_value_t = 0.6)]
    pacing: f64,
}

fn qpc_us32() -> u32 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: escriben en i64 locales.
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c.max(0) as u128 * 1_000_000 / f.max(1) as u128) as u64 as u32
}

/// Generador congruencial (reproducible, sin dependencias).
struct Lcg(u64);
impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Flecha azul con borde blanco y sombra semitransparente (32×32, hotspot 0,0).
fn arrow_shape() -> CursorShape {
    let (w, h) = (32u16, 32u16);
    let mut rgba = vec![0u8; usize::from(w) * usize::from(h) * 4];
    let inside = |x: f32, y: f32, grow: f32| {
        // Triángulo (0,0)-(0,24)-(17,18) en píxeles.
        x >= -grow
            && 18.0 * x - 17.0 * y <= grow * 20.0
            && 6.0 * x + 17.0 * (y - 24.0) <= grow * 20.0
    };
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (f32::from(x) + 0.5, f32::from(y) + 0.5);
            let px = if inside(fx, fy, 0.0) {
                [30, 90, 255, 255]
            } else if inside(fx, fy, 1.5) {
                [255, 255, 255, 255]
            } else if inside(fx - 3.0, fy - 3.0, 1.5) {
                [0, 0, 0, 96]
            } else {
                [0, 0, 0, 0]
            };
            let i = (usize::from(y) * usize::from(w) + usize::from(x)) * 4;
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    CursorShape {
        stream_id: 1,
        width: w,
        height: h,
        hotspot_x: 0,
        hotspot_y: 0,
        rgba,
    }
}

/// Mira verde de 49×49 con el punto caliente en el centro.
fn crosshair_shape() -> CursorShape {
    let n = 49u16;
    let mut rgba = vec![0u8; usize::from(n) * usize::from(n) * 4];
    for y in 0..n {
        for x in 0..n {
            let on = (21..=27).contains(&x) || (21..=27).contains(&y);
            let core = (23..=25).contains(&x) || (23..=25).contains(&y);
            let px = match (on, core) {
                (_, true) => [20, 220, 60, 255],
                (true, false) => [0, 0, 0, 200],
                _ => [0, 0, 0, 0],
            };
            let i = (usize::from(y) * usize::from(n) + usize::from(x)) * 4;
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    CursorShape {
        stream_id: 1,
        width: n,
        height: n,
        hotspot_x: 24,
        hotspot_y: 24,
        rgba,
    }
}

async fn hr_sleep_until(t: Instant) {
    let d = t.saturating_duration_since(Instant::now());
    if !d.is_zero() {
        let _ = tokio::task::spawn_blocking(move || cs_transport::os::high_res_sleep(d)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let data =
        std::fs::read(&args.file).with_context(|| format!("leyendo {}", args.file.display()))?;
    let aus: Vec<Vec<u8>> = split_access_units(&data)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    if aus.is_empty() || !is_idr(&aus[0]) {
        bail!("el fichero debe empezar por un IDR");
    }
    let idrs: Vec<usize> = (0..aus.len()).filter(|&i| is_idr(&aus[i])).collect();
    println!(
        "{}: {} AUs, {} IDR",
        args.file.display(),
        aus.len(),
        idrs.len()
    );

    let identity = Identity::load_or_create(&args.data_dir)?;
    let trust = TrustStore::open(args.data_dir.join("trusted-peers.json"))?;
    let prefix = args.peer.replace('-', "").to_lowercase();
    let peer = trust
        .fingerprints()
        .into_iter()
        .find(|fp| fp.to_string().starts_with(&prefix))
        .ok_or_else(|| anyhow!("ningún par emparejado coincide con {prefix}"))?;
    let endpoint = Endpoint::client("0.0.0.0:0".parse()?, identity.clone())?;
    let conn = endpoint
        .connect(args.addr, PeerPolicy::Exactly(peer), ALPN)
        .await?;
    let mut ctl = FramedStream::new(conn.open_bi().await?);
    ctl.send(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        device_id: identity.fingerprint().device_id(),
        device_name: "file_source".into(),
        roles: Roles {
            source: true,
            sink: false,
        },
    }))
    .await?;
    let mut sink = None;
    while sink.is_none() {
        match ctl.expect::<ControlMessage>().await? {
            ControlMessage::Hello(h) => println!(
                "visor: «{}» {}.{}",
                h.device_name, h.version.major, h.version.minor
            ),
            ControlMessage::SinkCapabilities(c) => sink = Some(c),
            other => println!("inesperado: {other:?}"),
        }
    }
    let sink = sink.expect("capacidades");
    println!(
        "SinkCapabilities: native_mode {}x{}@{} mHz, {} dpi, {:?}",
        sink.native_mode.width,
        sink.native_mode.height,
        sink.native_mode.refresh_mhz,
        sink.dpi,
        sink.decoders
    );
    ctl.send(&ControlMessage::StartStream(StartStream {
        stream_id: 1,
        mode: StreamMode::Duplicate,
        codec: Codec::H264,
        display_mode: Mode::new(1920, 1080, args.fps),
        bitrate_kbps: 20_000,
    }))
    .await?;
    loop {
        match ctl.expect::<ControlMessage>().await? {
            ControlMessage::StreamStarted { stream_id: 1 } => break,
            ControlMessage::StreamFailed { reason, .. } => bail!("StreamFailed: {reason}"),
            other => println!("esperando StreamStarted: {other:?}"),
        }
    }
    println!("StreamStarted recibido");

    let packetizer = Packetizer::new(
        1,
        PacketizerConfig {
            max_datagram: conn.max_datagram_size().unwrap_or(1200),
            fec_ratio: args.fec,
        },
    );
    let mut pacer = Pacer::new(PacerConfig {
        mode: PacingMode::Spread {
            fraction: args.pacing,
        },
        ..PacerConfig::default()
    });
    let interval = Duration::from_secs_f64(1.0 / f64::from(args.fps));
    // Reloj de frames con espera de alta resolución: `tokio::time` en Windows
    // tiene granularidad de ~15,6 ms y no llega a 60 fps estables.
    let mut next_frame = Instant::now();
    let mut rng = Lcg(0x5eed);
    let t0 = Instant::now();
    let (mut idx, mut seq) = (0usize, 0u32);
    let (mut forced, mut kf_requests, mut dropped_dg, mut dropped_frames, mut sent_dg) =
        (0u32, 0u32, 0u64, 0u32, 0u64);
    let mut want_idr = false;
    let mut shape_sent = 0;
    loop {
        tokio::select! {
            _ = hr_sleep_until(next_frame) => {
                next_frame += interval;
                if Instant::now() > next_frame + interval {
                    next_frame = Instant::now();
                }
                let t = t0.elapsed().as_secs_f64();
                if t >= args.seconds {
                    break;
                }
                // Cursor: forma al empezar y otra a los 5 s; posición en círculo.
                if shape_sent == 0 || (shape_sent == 1 && t >= 5.0) {
                    let shape = if shape_sent == 0 { arrow_shape() } else { crosshair_shape() };
                    ctl.send(&ControlMessage::CursorShape(shape)).await?;
                    shape_sent += 1;
                }
                let (cx, cy) = (960.0 + 400.0 * (t * 1.5).cos(), 540.0 + 300.0 * (t * 1.5).sin());
                ctl.send(&ControlMessage::CursorPosition { stream_id: 1, x: cx as i32, y: cy as i32, visible: true }).await?;

                if want_idr {
                    let pos = idx % aus.len();
                    let base = idx - pos;
                    let last = idrs.iter().rev().find(|&&i| i <= pos).copied().unwrap_or(0);
                    idx = base + last;
                    forced += 1;
                    want_idr = false;
                }
                let au = &aus[idx % aus.len()];
                let meta = FrameMeta { frame_seq: seq, pts_us: qpc_us32(), keyframe: is_idr(au), discardable: false };
                let dgs = packetizer.packetize(meta, au).map_err(|e| anyhow!("packetize: {e:?}"))?;
                let drop_frame = args.drop_every > 0 && seq % args.drop_every == args.drop_every - 1;
                if drop_frame {
                    dropped_frames += 1;
                }
                let kept: Vec<Bytes> = dgs
                    .into_iter()
                    .filter(|_| {
                        let drop = drop_frame || (args.loss > 0.0 && rng.next_f64() < args.loss);
                        if drop {
                            dropped_dg += 1;
                        }
                        !drop
                    })
                    .map(Bytes::from)
                    .collect();
                sent_dg += kept.len() as u64;
                let sizes: Vec<usize> = kept.iter().map(Bytes::len).collect();
                let schedule = pacer.schedule(&sizes, interval, Instant::now());
                let c = conn.clone();
                send_paced(kept, schedule, PacedSendOptions::default(), move |d| c.send_datagram(d).is_ok()).await;
                idx += 1;
                seq += 1;
            }
            m = ctl.recv::<ControlMessage>() => match m? {
                Some(ControlMessage::RequestKeyframe { .. }) => {
                    kf_requests += 1;
                    want_idr = true;
                }
                Some(ControlMessage::ReceiverStats(s)) => println!("ReceiverStats: {s:?}"),
                Some(ControlMessage::Bye { reason }) => {
                    println!("Bye del visor: {reason:?}");
                    break;
                }
                Some(other) => println!("control: {other:?}"),
                None => break,
            }
        }
    }
    let _ = ctl
        .send(&ControlMessage::Bye {
            reason: ByeReason::UserRequested,
        })
        .await;
    ctl.finish();
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(b"bye");
    endpoint.close();
    println!(
        "file_source: {seq} frames enviados en {:.1} s, {sent_dg} datagramas, {dropped_dg} descartados a propósito ({dropped_frames} frames enteros), {kf_requests} RequestKeyframe recibidos, {forced} IDR forzados",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
