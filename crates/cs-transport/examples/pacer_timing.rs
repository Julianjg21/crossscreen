//! Mide la precisión y el coste de CPU de cada estrategia de espera del
//! *pacer*, sin red: el "envío" solo anota el instante real.
//!
//! Uso: `cargo run --release -p cs-transport --example pacer_timing -- [mbps] [fps] [fracción] [segundos]`

use bytes::Bytes;
use cs_transport::pacer::{PacedSendOptions, Pacer, PacerConfig, PacingMode, WaitStrategy};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DATAGRAM: usize = 1200;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: f64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (mbps, fps, fraction, seconds) = (arg(0, 80.0), arg(1, 60.0), arg(2, 0.6), arg(3, 5.0));
    let interval = Duration::from_secs_f64(1.0 / fps);
    let frame_bytes = mbps * 1e6 / 8.0 / fps * 1.1; // +10 % FEC
    let n = (frame_bytes / (DATAGRAM - 24) as f64).ceil() as usize;
    let frames = (fps * seconds) as usize;
    println!(
        "{mbps} Mbps, {fps} fps, reparto {:.0} % ({:.2} ms), {n} datagramas/frame, {frames} frames",
        fraction * 100.0,
        interval.as_secs_f64() * fraction * 1e3
    );
    println!("estrategia | retraso p50 | p99 | máx | fin de frame medio | esperas/frame | CPU");

    for wait in [
        WaitStrategy::HighResTimer,
        WaitStrategy::ThreadSleep,
        WaitStrategy::YieldSpin,
        WaitStrategy::TokioSleep,
    ] {
        let mut pacer = Pacer::new(PacerConfig {
            mode: PacingMode::Spread { fraction },
            burst_bytes: 4 * DATAGRAM,
        });
        let opts = PacedSendOptions {
            wait,
            quantum: Duration::from_micros(250),
        };
        let mut late_us: Vec<i64> = Vec::new();
        let mut frame_end_us = 0f64;
        let mut waits = 0usize;
        let cpu0 = cs_transport::os::process_cpu_time();
        let wall0 = Instant::now();
        let mut ticker = tokio::time::interval(interval);
        for _ in 0..frames {
            ticker.tick().await;
            let now = Instant::now();
            let schedule = pacer.schedule(&vec![DATAGRAM; n], interval, now);
            let dgs: Vec<Bytes> = (0..n).map(|_| Bytes::from_static(&[0u8; 8])).collect();
            let log = Arc::new(Mutex::new(Vec::with_capacity(n)));
            let l = log.clone();
            let r = cs_transport::pacer::send_paced(dgs, schedule.clone(), opts, move |_| {
                l.lock().unwrap().push(Instant::now());
                true
            })
            .await;
            waits += r.waits;
            let log = log.lock().unwrap();
            for (at, due) in log.iter().zip(&schedule) {
                late_us.push(if at >= due {
                    (*at - *due).as_micros() as i64
                } else {
                    -((*due - *at).as_micros() as i64)
                });
            }
            frame_end_us += (*log.last().unwrap() - now).as_secs_f64() * 1e6;
        }
        let wall = wall0.elapsed();
        let cpu = match (cpu0, cs_transport::os::process_cpu_time()) {
            (Some(a), Some(b)) => format!(
                "{:.1} %",
                (b - a).as_secs_f64() / wall.as_secs_f64() * 100.0
            ),
            _ => "-".into(),
        };
        late_us.sort_unstable();
        let q = |p: f64| late_us[((late_us.len() - 1) as f64 * p).round() as usize];
        println!(
            "{wait:?} | {} µs | {} µs | {} µs | {:.2} ms | {:.1} | {cpu}",
            q(0.5),
            q(0.99),
            q(1.0),
            frame_end_us / frames as f64 / 1e3,
            waits as f64 / frames as f64
        );
    }
}
