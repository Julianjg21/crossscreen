//! Emisor de prueba **desechable** para probar `crossscreen-viewer --listen`
//! sin el emisor real: habla el contrato de `docs/09-contrato-sesion-mvp.md`
//! y transmite las AU de un fichero H.264 Annex-B con `pts_us` = QPC actual,
//! más mensajes de cursor sintéticos (forma y posición en círculo).
//!
//! Para probar la recuperación se pueden inducir pérdidas: `--loss P`
//! descarta cada datagrama con probabilidad P; `--drop-every N` descarta
//! todos los datagramas de uno de cada N frames; `--ge-p/--ge-r/--ge-loss`
//! activan un modelo de pérdidas **en ráfaga** de Gilbert-Elliott por
//! datagrama (aproxima el Wi-Fi: casi siempre bien, de vez en cuando unos
//! datagramas seguidos perdidos). `--seed` fija la semilla. Ante `RequestKeyframe` el
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
use cs_transport::fec::{AdaptiveFec, AdaptiveFecConfig};
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
    /// Gilbert-Elliott: probabilidad por datagrama de pasar de «bueno» a
    /// «malo» (0 = modelo desactivado).
    #[arg(long, default_value_t = 0.0)]
    ge_p: f64,
    /// Gilbert-Elliott: probabilidad por datagrama de volver de «malo» a
    /// «bueno» (duración media de la ráfaga = 1/r datagramas).
    #[arg(long, default_value_t = 0.25)]
    ge_r: f64,
    /// Gilbert-Elliott: probabilidad de perder un datagrama en estado «malo».
    #[arg(long, default_value_t = 0.8)]
    ge_loss: f64,
    /// Semilla del generador de pérdidas (misma semilla = mismo proceso).
    #[arg(long, default_value_t = 0x5eed)]
    seed: u64,
    #[arg(long, default_value_t = 0.1)]
    fec: f32,
    /// Paridad FEC separada para IDR (`None` = misma que `--fec`).
    #[arg(long)]
    keyframe_fec: Option<f32>,
    /// Entrelazar shards entre bloques FEC de un mismo frame.
    #[arg(long)]
    interleave: bool,
    /// Ajustar la paridad FEC según `ReceiverStats`.
    #[arg(long)]
    adaptive_fec: bool,
    /// Coalescencia de `RequestKeyframe` en ms (0 = sin filtro). Con > 0
    /// también ignora peticiones recibidas a menos de 1 RTT del último IDR.
    #[arg(long, default_value_t = 0)]
    coalesce_ms: u64,
    /// Cota inferior de RTT simulado (ms) para la ventana de ignorar < 1 RTT.
    #[arg(long, default_value_t = 0)]
    sim_rtt_ms: u64,
    #[arg(long, default_value_t = 0.6)]
    pacing: f64,
}

#[cfg(not(windows))]
fn qpc_us32() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u32
}

#[cfg(windows)]
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

/// Canal de Gilbert-Elliott de dos estados. Pérdida media esperada:
/// `p / (p + r) · loss_bad`.
struct GilbertElliott {
    p: f64,
    r: f64,
    loss_bad: f64,
    bad: bool,
    bursts: u64,
}

impl GilbertElliott {
    /// `true` si el datagrama se pierde.
    fn drop(&mut self, rng: &mut Lcg) -> bool {
        if self.bad {
            if rng.next_f64() < self.r {
                self.bad = false;
            }
        } else if rng.next_f64() < self.p {
            self.bad = true;
            self.bursts += 1;
        }
        self.bad && rng.next_f64() < self.loss_bad
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

    let mut packetizer = Packetizer::new(
        1,
        PacketizerConfig {
            max_datagram: conn.max_datagram_size().unwrap_or(1200),
            fec_ratio: args.fec,
        },
    )
    .with_keyframe_fec_ratio(args.keyframe_fec.unwrap_or(args.fec))
    .with_interleave(args.interleave);
    let mut adaptive_fec = args.adaptive_fec.then(|| {
        AdaptiveFec::new(AdaptiveFecConfig {
            initial: args.fec,
            ..AdaptiveFecConfig::default()
        })
    });
    let mut fec_changes = 0u32;
    let mut fec_max = args.fec;
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
    let mut rng = Lcg(args.seed);
    // Generador aparte para el canal: el proceso de pérdidas no depende de
    // qué otros sorteos se hagan.
    let mut ge_rng = Lcg(args.seed ^ 0x9e37_79b9_7f4a_7c15);
    let mut ge = (args.ge_p > 0.0).then_some(GilbertElliott {
        p: args.ge_p,
        r: args.ge_r,
        loss_bad: args.ge_loss,
        bad: false,
        bursts: 0,
    });
    if let Some(g) = &ge {
        println!(
            "Gilbert-Elliott p={} r={} pérdida en malo={}: pérdida media esperada {:.2} %, ráfaga media {:.1} datagramas",
            g.p,
            g.r,
            g.loss_bad,
            100.0 * g.p / (g.p + g.r) * g.loss_bad,
            1.0 / g.r
        );
    }
    let t0 = Instant::now();
    let (mut idx, mut seq) = (0usize, 0u32);
    let (
        mut forced,
        mut kf_requests,
        mut kf_ignored_rtt,
        mut kf_coalesced,
        mut dropped_dg,
        mut dropped_frames,
        mut sent_dg,
    ) = (0u32, 0u32, 0u32, 0u32, 0u64, 0u32, 0u64);
    let mut want_idr = false;
    let mut last_forced_at: Option<Instant> = None;
    let mut deferred_idr_until: Option<Instant> = None;
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

                let now = Instant::now();
                if deferred_idr_until.is_some_and(|due| now >= due) {
                    deferred_idr_until = None;
                    want_idr = true;
                }
                if want_idr {
                    // El fichero no permite forzar un IDR nuevo: se salta al
                    // IDR más reciente (la imagen retrocede, pero es un punto
                    // de acceso limpio y del tamaño de un IDR real).
                    let pos = idx % aus.len();
                    let base = idx - pos;
                    let last = idrs.iter().rev().find(|&&i| i <= pos).copied().unwrap_or(0);
                    idx = base + last;
                    forced += 1;
                    last_forced_at = Some(now);
                    want_idr = false;
                }
                let au = &aus[idx % aus.len()];
                if is_idr(au) && last_forced_at.is_none() {
                    last_forced_at = Some(now);
                }
                let meta = FrameMeta { frame_seq: seq, pts_us: qpc_us32(), keyframe: is_idr(au), discardable: false };
                let dgs = packetizer.packetize(meta, au).map_err(|e| anyhow!("packetize: {e:?}"))?;
                let drop_frame = args.drop_every > 0 && seq % args.drop_every == args.drop_every - 1;
                if drop_frame {
                    dropped_frames += 1;
                }
                let kept: Vec<Bytes> = dgs
                    .into_iter()
                    .filter(|_| {
                        let burst = ge.as_mut().is_some_and(|g| g.drop(&mut ge_rng));
                        let drop = burst
                            || drop_frame
                            || (args.loss > 0.0 && rng.next_f64() < args.loss);
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
                    if args.coalesce_ms == 0 {
                        want_idr = true;
                    } else {
                        let now = Instant::now();
                        let rtt = conn.rtt().max(Duration::from_millis(args.sim_rtt_ms)).min(Duration::from_millis(500));
                        let min_interval = Duration::from_millis(args.coalesce_ms);
                        match last_forced_at {
                            Some(last) if now.saturating_duration_since(last) < rtt => {
                                kf_ignored_rtt += 1;
                            }
                            Some(last) if now.saturating_duration_since(last) < min_interval => {
                                kf_coalesced += 1;
                                deferred_idr_until.get_or_insert(last + min_interval);
                            }
                            _ => {
                                deferred_idr_until = None;
                                want_idr = true;
                            }
                        }
                    }
                }
                Some(ControlMessage::ReceiverStats(s)) => {
                    println!("ReceiverStats: {s:?}");
                    if let Some(af) = adaptive_fec.as_mut() {
                        if let Some(ch) = af.on_stats(
                            s.frames_received,
                            s.frames_lost,
                            s.pre_fec_loss_bp,
                        ) {
                            packetizer.set_fec_ratio(ch.to);
                            fec_changes += 1;
                            fec_max = fec_max.max(ch.to);
                            println!("AdaptiveFec: {:.2} -> {:.2} ({:?})", ch.from, ch.to, ch.reason);
                        }
                    }
                }
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
    let total_dg = sent_dg + dropped_dg;
    println!(
        "pérdida efectiva: {dropped_dg}/{total_dg} datagramas ({:.2} %), {} ráfagas Gilbert-Elliott",
        100.0 * dropped_dg as f64 / total_dg.max(1) as f64,
        ge.as_ref().map_or(0, |g| g.bursts)
    );
    println!(
        "file_source: {seq} frames enviados en {:.1} s, {sent_dg} datagramas, {dropped_dg} descartados a propósito ({dropped_frames} frames enteros), {kf_requests} RequestKeyframe recibidos ({kf_ignored_rtt} ignorados <1 RTT, {kf_coalesced} coalescidos), {forced} IDR forzados, FEC final {:.2} (máx {:.2}, {fec_changes} ajustes)",
        t0.elapsed().as_secs_f64(),
        packetizer.config().fec_ratio,
        fec_max
    );
    Ok(())
}
