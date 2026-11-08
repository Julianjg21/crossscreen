//! Banco de pruebas del transporte de vídeo (PoC-4).
//!
//! El emisor genera frames **sintéticos** del tamaño que correspondería al
//! bitrate y fps pedidos (con un "IDR" 4× mayor cada 2 s), los fragmenta con
//! FEC y los envía por datagramas QUIC. El receptor reensambla y confirma cada
//! frame por el stream de control. El emisor mide el *frame RTT*: desde el
//! envío del primer datagrama del frame hasta recibir su confirmación. Esto
//! incluye red (ida y vuelta), reensamblado y FEC; no incluye captura,
//! codificación ni decodificación.
//!
//! *Pacing* (`--pacing`): por defecto cada frame se reparte en el 60 % del
//! intervalo entre frames con `cs_transport::pacer` (presupuesto de bytes +
//! temporizador de alta resolución); `--pacing off` lo envía en ráfaga.

use anyhow::{Context, Result};
use bytes::Bytes;
use cs_protocol::framing::{self, FrameDecoder};
use cs_protocol::ALPN;
use cs_security::identity::Identity;
use cs_security::trust::TrustStore;
use cs_security::verifier::PeerPolicy;
use cs_security::Fingerprint;
use cs_transport::pacer::{self, PacedSendOptions, Pacer, PacerConfig, PacingMode, WaitStrategy};
use cs_transport::packetizer::FrameMeta;
use cs_transport::{
    Connection, Endpoint, Packetizer, PacketizerConfig, Reassembler, ReassemblyEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
enum BenchMsg {
    Start { fps: u32 },
    Ack { seq: u32, recovered: u32 },
    Lost { seq: u32 },
    End,
    Summary(Summary),
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct Summary {
    datagrams: u64,
    frames_delivered: u64,
    frames_lost: u64,
    shards_recovered: u64,
    late: u64,
    invalid: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub mbps: f64,
    pub fps: u32,
    pub seconds: u64,
    pub fec: f32,
    pub pacing: PacingArgs,
}

/// Opciones de *pacing* del emisor (se aplanan en el subcomando `bench`).
#[derive(Debug, Clone, Copy, clap::Args)]
pub struct PacingArgs {
    /// Fracción del intervalo entre frames en la que repartir cada frame
    /// (0 < f ≤ 1), u `off` para enviarlo en ráfaga.
    #[arg(long, default_value = "0.6", value_parser = parse_pacing)]
    pub pacing: PacingMode,
    /// Cómo esperar entre micro-ráfagas: highres | thread | yield | tokio.
    #[arg(long, default_value = "highres")]
    pub pacing_wait: WaitStrategy,
}

fn parse_pacing(s: &str) -> Result<PacingMode, String> {
    if s.eq_ignore_ascii_case("off") {
        return Ok(PacingMode::Off);
    }
    match s.parse::<f64>() {
        Ok(fraction) if fraction > 0.0 && fraction <= 1.0 => Ok(PacingMode::Spread { fraction }),
        _ => Err(format!(
            "--pacing espera una fracción en (0, 1] u `off`, no `{s}`"
        )),
    }
}

async fn write_msg(send: &mut quinn::SendStream, msg: &BenchMsg) -> Result<()> {
    send.write_all(&framing::encode(msg)?).await?;
    Ok(())
}

async fn read_msg(
    recv: &mut quinn::RecvStream,
    dec: &mut FrameDecoder,
    buf: &mut [u8],
) -> Result<Option<BenchMsg>> {
    loop {
        if let Some(m) = dec.next_frame()? {
            return Ok(Some(m));
        }
        match recv.read(buf).await? {
            Some(n) => dec.push(&buf[..n]),
            None => return Ok(None),
        }
    }
}

// ───────────────────────────── receptor ─────────────────────────────

pub async fn serve(identity: &Identity, trust: &TrustStore, bind: SocketAddr) -> Result<()> {
    let fps = trust.fingerprints();
    if fps.is_empty() {
        anyhow::bail!("no hay equipos emparejados; empareja primero con `pair-listen` / `pair`");
    }
    let endpoint = Endpoint::server(bind, identity.clone(), PeerPolicy::pinned(fps), ALPN)?;
    println!(
        "bench-serve escuchando en UDP {} (solo pares emparejados)",
        endpoint.local_addr()?
    );
    while let Some(conn) = endpoint.accept().await {
        match conn {
            Ok(conn) => {
                println!(
                    "Conexión de {} ({})",
                    conn.remote_addr(),
                    conn.peer_fingerprint().short()
                );
                tokio::spawn(async move {
                    if let Err(e) = handle_bench_conn(conn).await {
                        eprintln!("sesión de banco terminada con error: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("conexión rechazada: {e}"),
        }
    }
    Ok(())
}

async fn handle_bench_conn(conn: Connection) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; 4096];
    match read_msg(&mut recv, &mut dec, &mut buf).await? {
        Some(BenchMsg::Start { fps }) => println!("  inicio: {fps} fps"),
        other => anyhow::bail!("mensaje inicial inesperado: {other:?}"),
    }
    let (end_tx, mut end_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        while let Ok(Some(m)) = read_msg(&mut recv, &mut dec, &mut buf).await {
            if matches!(m, BenchMsg::End) {
                let _ = end_tx.send(());
                return;
            }
        }
    });

    let mut r = Reassembler::new(1);
    loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let d = d?;
                let events = match r.push(&d) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                for ev in events {
                    let msg = match ev {
                        ReassemblyEvent::Frame(f) => BenchMsg::Ack { seq: f.frame_seq, recovered: f.recovered_shards },
                        ReassemblyEvent::Lost { frame_seq } => BenchMsg::Lost { seq: frame_seq },
                    };
                    write_msg(&mut send, &msg).await?;
                }
            }
            _ = &mut end_rx => break,
        }
    }
    let s = r.stats();
    let summary = Summary {
        datagrams: s.datagrams,
        frames_delivered: s.frames_delivered,
        frames_lost: s.frames_lost,
        shards_recovered: s.shards_recovered,
        late: s.late_datagrams,
        invalid: s.invalid_datagrams,
    };
    write_msg(&mut send, &BenchMsg::Summary(summary)).await?;
    let _ = send.finish();
    println!("  fin: {summary:?}");
    let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    Ok(())
}

// ───────────────────────────── emisor ─────────────────────────────

#[derive(Default)]
struct AckState {
    sent_at: HashMap<u32, Instant>,
    frame_rtts_us: Vec<u64>,
    /// Instante de cada ACK, para medir también desde el último datagrama.
    acked_at: HashMap<u32, Instant>,
    lost: u64,
    recovered: u64,
    summary: Option<Summary>,
}

pub async fn run(
    identity: &Identity,
    addr: SocketAddr,
    peer: Fingerprint,
    p: Params,
) -> Result<()> {
    let bind: SocketAddr = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()?;
    let endpoint = Endpoint::client(bind, identity.clone())?;
    let conn = endpoint
        .connect(addr, PeerPolicy::Exactly(peer), ALPN)
        .await
        .context("conectando al receptor")?;
    let max_dg = conn
        .max_datagram_size()
        .context("el par no admite datagramas")?;
    println!(
        "Conectado a {} — datagrama máx. {max_dg} B, RTT {:?}",
        conn.remote_addr(),
        conn.rtt()
    );

    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &BenchMsg::Start { fps: p.fps }).await?;

    let state = Arc::new(Mutex::new(AckState::default()));
    let st = state.clone();
    let reader = tokio::spawn(async move {
        let mut dec = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(Some(m)) = read_msg(&mut recv, &mut dec, &mut buf).await {
            let mut s = st.lock().expect("mutex");
            match m {
                BenchMsg::Ack { seq, recovered } => {
                    if let Some(t) = s.sent_at.remove(&seq) {
                        s.frame_rtts_us.push(t.elapsed().as_micros() as u64);
                        s.acked_at.insert(seq, Instant::now());
                    }
                    s.recovered += u64::from(recovered);
                }
                BenchMsg::Lost { seq } => {
                    s.sent_at.remove(&seq);
                    s.lost += 1;
                }
                BenchMsg::Summary(sum) => {
                    s.summary = Some(sum);
                    return;
                }
                _ => {}
            }
        }
    });

    let packetizer = Packetizer::new(
        1,
        PacketizerConfig {
            max_datagram: max_dg,
            fec_ratio: p.fec,
        },
    );
    let frame_bytes = ((p.mbps * 1e6 / 8.0) / f64::from(p.fps)).max(64.0) as usize;
    let key_interval = p.fps * 2;
    let pool: Vec<u8> = (0..frame_bytes * 4)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761) as u8)
        .collect();

    let total_frames = p.fps as u64 * p.seconds;
    let interval = Duration::from_secs_f64(1.0 / f64::from(p.fps));
    let mut pacer = Pacer::new(PacerConfig {
        mode: p.pacing.pacing,
        burst_bytes: 4 * max_dg,
    });
    let send_opts = PacedSendOptions {
        wait: p.pacing.pacing_wait,
        ..PacedSendOptions::default()
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let cpu_start = cs_transport::os::process_cpu_time();
    let start = Instant::now();
    let (mut bytes_sent, mut dg_sent, mut dg_failed) = (0u64, 0u64, 0u64);
    let mut max_late = Duration::ZERO;
    let mut frame_send_us: Vec<u64> = Vec::with_capacity(total_frames as usize);
    let mut last_sent_at: HashMap<u32, Instant> = HashMap::new();

    for seq in 0..total_frames as u32 {
        ticker.tick().await;
        let keyframe = seq % key_interval == 0;
        let len = if keyframe {
            frame_bytes * 4
        } else {
            frame_bytes
        };
        let meta = FrameMeta {
            frame_seq: seq,
            pts_us: start.elapsed().as_micros() as u32,
            keyframe,
            discardable: false,
        };
        let dgs = packetizer.packetize(meta, &pool[..len])?;
        let sizes: Vec<usize> = dgs.iter().map(Vec::len).collect();
        bytes_sent += sizes.iter().sum::<usize>() as u64;
        let t_frame = Instant::now();
        state.lock().expect("mutex").sent_at.insert(seq, t_frame);
        if matches!(pacer.config().mode, PacingMode::Off) {
            for d in dgs {
                match conn.send_datagram(Bytes::from(d)) {
                    Ok(()) => dg_sent += 1,
                    Err(_) => dg_failed += 1,
                }
            }
            last_sent_at.insert(seq, Instant::now());
        } else {
            let schedule = pacer.schedule(&sizes, interval, t_frame);
            let c = conn.clone();
            let r = pacer::send_paced(
                dgs.into_iter().map(Bytes::from).collect(),
                schedule,
                send_opts,
                move |d| c.send_datagram(d).is_ok(),
            )
            .await;
            dg_sent += r.sent as u64;
            dg_failed += r.failed as u64;
            max_late = max_late.max(r.max_late);
            if let Some(t) = r.last_sent_at {
                last_sent_at.insert(seq, t);
            }
        }
        frame_send_us.push(t_frame.elapsed().as_micros() as u64);
    }
    let elapsed = start.elapsed();
    let cpu = cpu_start
        .zip(cs_transport::os::process_cpu_time())
        .map(|(a, b)| (b - a).as_secs_f64() / elapsed.as_secs_f64() * 100.0);
    // Margen para que lleguen los últimos ACK.
    tokio::time::sleep(Duration::from_millis(300)).await;
    write_msg(&mut send, &BenchMsg::End).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), reader).await;

    let stats = conn.stats();
    let s = state.lock().expect("mutex");
    let mut rtts = s.frame_rtts_us.clone();
    rtts.sort_unstable();
    let pct = |q: f64| -> String {
        if rtts.is_empty() {
            return "-".into();
        }
        let i = ((rtts.len() as f64 - 1.0) * q).round() as usize;
        format!("{:.2} ms", rtts[i] as f64 / 1000.0)
    };

    println!();
    println!("── Resultado ({:.1} s) ──", elapsed.as_secs_f64());
    println!(
        "Frames enviados:      {total_frames} ({:.1} fps efectivos)",
        total_frames as f64 / elapsed.as_secs_f64()
    );
    println!("Frames confirmados:   {}", rtts.len());
    println!("Frames perdidos:      {} (tras FEC)", s.lost);
    println!("Sin respuesta:        {}", s.sent_at.len());
    println!(
        "Shards recuperados:   {} (FEC {:.0} %)",
        s.recovered,
        p.fec * 100.0
    );
    println!(
        "Throughput (cabecera incl.): {:.2} Mbps",
        bytes_sent as f64 * 8.0 / elapsed.as_secs_f64() / 1e6
    );
    println!("Datagramas:           {dg_sent} enviados, {dg_failed} rechazados localmente");
    println!(
        "Frame RTT p50/p95/p99/máx: {} / {} / {} / {}",
        pct(0.5),
        pct(0.95),
        pct(0.99),
        pct(1.0)
    );
    // Con pacing el frame RTT incluye la ventana de reparto; desde el último
    // datagrama se ve solo la cola de red + reensamblado.
    let mut tail_us: Vec<u64> = s
        .acked_at
        .iter()
        .filter_map(|(seq, ack)| {
            last_sent_at
                .get(seq)
                .map(|t| ack.saturating_duration_since(*t).as_micros() as u64)
        })
        .collect();
    tail_us.sort_unstable();
    let tail_pct = |q: f64| {
        tail_us
            .get(((tail_us.len().max(1) - 1) as f64 * q).round() as usize)
            .map_or("-".into(), |&v| format!("{:.2} ms", v as f64 / 1000.0))
    };
    println!(
        "RTT desde último datagrama p50/p95/p99: {} / {} / {}",
        tail_pct(0.5),
        tail_pct(0.95),
        tail_pct(0.99)
    );
    frame_send_us.sort_unstable();
    let send_pct = |q: f64| {
        frame_send_us
            .get(((frame_send_us.len().max(1) - 1) as f64 * q).round() as usize)
            .map_or("-".into(), |&v| format!("{:.2} ms", v as f64 / 1000.0))
    };
    match p.pacing.pacing {
        PacingMode::Spread { fraction: f } => println!(
            "Pacing:               {:.0} % del intervalo ({:.2} ms), espera {:?}, retraso máx. {:.2} ms",
            f * 100.0,
            interval.as_secs_f64() * f * 1000.0,
            p.pacing.pacing_wait,
            max_late.as_secs_f64() * 1000.0
        ),
        PacingMode::Off => println!("Pacing:               desactivado (ráfaga por frame)"),
        PacingMode::Rate { bytes_per_sec } => println!(
            "Pacing:               {:.1} Mbps fijos",
            bytes_per_sec * 8.0 / 1e6
        ),
    }
    println!(
        "Duración envío frame p50/p99: {} / {}",
        send_pct(0.5),
        send_pct(0.99)
    );
    match cpu {
        Some(c) => println!(
            "CPU del emisor:       {c:.1} % de un núcleo (usuario + núcleo, proceso entero)"
        ),
        None => println!("CPU del emisor:       no disponible en esta plataforma"),
    }
    println!(
        "QUIC: RTT {:?}, paquetes perdidos {}, eventos de congestión {}, cwnd {} B",
        stats.path.rtt, stats.path.lost_packets, stats.path.congestion_events, stats.path.cwnd
    );
    match s.summary {
        Some(sum) => println!("Receptor: {sum:?}"),
        None => println!("Receptor: no envió resumen"),
    }
    conn.close(b"bench done");
    endpoint.close();
    Ok(())
}
