//! *Pacing* de los datagramas de vídeo dentro de un frame.
//!
//! Enviar un frame entero en ráfaga desborda los *buffers* del AP Wi-Fi y de
//! los conmutadores; repartirlo en el ~50–70 % del intervalo entre frames
//! (`docs/04-protocolos-streaming.md` §4.4) suaviza la cola sin añadir apenas
//! latencia.
//!
//! El módulo tiene dos capas:
//!
//! - **Sans-IO** ([`TokenBucket`], [`Pacer`]): dado el tamaño de cada
//!   datagrama, calcula el instante de envío de cada uno con un presupuesto de
//!   bytes (*token bucket*). Es puro: el tiempo se inyecta como `Instant`, así
//!   que los tests son deterministas.
//! - **Ejecución** ([`send_paced`]): envía siguiendo el plan en
//!   *micro-ráfagas*: al despertar envía todo lo que vence antes de
//!   `ahora + quantum` y vuelve a esperar. Así la precisión necesaria del
//!   temporizador es `quantum` (250 µs por defecto), no el espaciado entre
//!   datagramas (~70 µs a 80 Mbps).
//!
//! ## Cómo se espera ([`WaitStrategy`])
//!
//! En Windows `tokio::time::sleep` redondea a milisegundos y, según la
//! resolución del temporizador del sistema, despierta con 1–15,6 ms de
//! retraso: inútil para repartir un frame en ~10 ms. Se midieron cuatro
//! estrategias (ejemplo `pacer_timing`, resultados en
//! `docs/poc-results/poc-04-transporte.md`):
//!
//! - [`WaitStrategy::HighResTimer`] (**por defecto**): el frame se envía desde
//!   un hilo de `spawn_blocking` que duerme con un temporizador *waitable* de
//!   alta resolución (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`). Precisión
//!   sub-milisegundo con CPU casi nula. `quinn::Connection::send_datagram` es
//!   síncrono y seguro entre hilos, así que no hace falta volver al runtime.
//! - [`WaitStrategy::ThreadSleep`]: igual pero con `std::thread::sleep`.
//! - [`WaitStrategy::YieldSpin`]: espera activa con `tokio::task::yield_now`.
//!   Muy precisa, pero ocupa un núcleo durante toda la ventana de reparto.
//! - [`WaitStrategy::TokioSleep`]: solo como referencia.

use bytes::Bytes;
use std::time::{Duration, Instant};

/// Cómo repartir un frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PacingMode {
    /// Sin *pacing*: todo el frame en ráfaga.
    Off,
    /// Repartir cada frame en `fraction` del intervalo entre frames
    /// (p. ej. 0,6). El ritmo se recalcula por frame según su tamaño.
    Spread { fraction: f64 },
    /// Ritmo fijo en bytes por segundo, independiente del tamaño del frame.
    Rate { bytes_per_sec: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacerConfig {
    pub mode: PacingMode,
    /// Capacidad del *bucket*: bytes que pueden salir seguidos sin esperar
    /// (como mínimo se permite siempre un datagrama completo).
    pub burst_bytes: usize,
}

impl Default for PacerConfig {
    fn default() -> Self {
        PacerConfig {
            mode: PacingMode::Spread { fraction: 0.6 },
            burst_bytes: 4 * 1200,
        }
    }
}

/// *Token bucket* en bytes con tiempo inyectado.
///
/// Si un envío supera la capacidad, se espera a tener el bucket lleno y el
/// saldo queda en negativo (deuda), que se paga antes del siguiente envío.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// Bucket lleno en `now`. `rate` en bytes/s (> 0).
    pub fn new(rate: f64, capacity: usize, now: Instant) -> Self {
        let capacity = capacity.max(1) as f64;
        TokenBucket {
            rate: rate.max(1.0),
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn set_rate(&mut self, rate: f64) {
        self.rate = rate.max(1.0);
    }

    /// Saldo disponible en `at` (puede ser negativo si hay deuda).
    pub fn tokens_at(&self, at: Instant) -> f64 {
        let dt = at.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + dt * self.rate).min(self.capacity)
    }

    /// Reserva `bytes` y devuelve el instante (≥ `now` y ≥ la reserva
    /// anterior) en que pueden enviarse.
    pub fn reserve(&mut self, bytes: usize, now: Instant) -> Instant {
        let t0 = now.max(self.last);
        let available = self.tokens_at(t0);
        let need = (bytes as f64).min(self.capacity);
        let at = if available >= need {
            t0
        } else {
            t0 + Duration::from_secs_f64((need - available) / self.rate)
        };
        self.tokens = self.tokens_at(at) - bytes as f64;
        self.last = at;
        at
    }
}

/// Planificador de envío por frame (sans-IO). Conserva el bucket entre
/// frames: si un frame grande aún no ha terminado de salir, el siguiente
/// empieza detrás de él en vez de solaparse.
#[derive(Debug, Clone)]
pub struct Pacer {
    cfg: PacerConfig,
    bucket: Option<TokenBucket>,
}

impl Pacer {
    pub fn new(cfg: PacerConfig) -> Self {
        Pacer { cfg, bucket: None }
    }

    pub fn config(&self) -> PacerConfig {
        self.cfg
    }

    /// Instante de envío de cada datagrama de un frame (mismo orden que
    /// `sizes`). `frame_interval` es 1/fps.
    pub fn schedule(
        &mut self,
        sizes: &[usize],
        frame_interval: Duration,
        now: Instant,
    ) -> Vec<Instant> {
        let total: usize = sizes.iter().sum();
        let rate = match self.cfg.mode {
            PacingMode::Off => return vec![now; sizes.len()],
            PacingMode::Spread { fraction } => {
                let window = frame_interval.as_secs_f64() * fraction.clamp(0.01, 1.0);
                // Los primeros `burst_bytes` salen al instante: el resto se
                // reparte para acabar justo al final de la ventana.
                let paced = total.saturating_sub(self.capacity(sizes)) as f64;
                if window <= 0.0 || paced <= 0.0 {
                    f64::MAX / 4.0
                } else {
                    paced / window
                }
            }
            PacingMode::Rate { bytes_per_sec } => bytes_per_sec,
        };
        let capacity = self.capacity(sizes);
        let bucket = self
            .bucket
            .get_or_insert_with(|| TokenBucket::new(rate, capacity, now));
        bucket.set_rate(rate);
        sizes.iter().map(|&s| bucket.reserve(s, now)).collect()
    }

    fn capacity(&self, sizes: &[usize]) -> usize {
        let max_dg = sizes.iter().copied().max().unwrap_or(0);
        self.cfg.burst_bytes.max(max_dg)
    }
}

/// Fin (exclusivo) de la micro-ráfaga que empieza en `from`: todos los
/// datagramas que vencen antes de `now + quantum`.
pub fn burst_end(schedule: &[Instant], from: usize, now: Instant, quantum: Duration) -> usize {
    let horizon = now + quantum;
    from + schedule[from..]
        .iter()
        .take_while(|&&t| t <= horizon)
        .count()
}

/// Cómo esperar entre micro-ráfagas. Ver la documentación del módulo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitStrategy {
    #[default]
    HighResTimer,
    ThreadSleep,
    YieldSpin,
    TokioSleep,
}

impl std::str::FromStr for WaitStrategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "highres" => Ok(Self::HighResTimer),
            "thread" => Ok(Self::ThreadSleep),
            "yield" => Ok(Self::YieldSpin),
            "tokio" => Ok(Self::TokioSleep),
            _ => Err(format!(
                "estrategia de espera desconocida: {s} (highres|thread|yield|tokio)"
            )),
        }
    }
}

/// Opciones de ejecución de [`send_paced`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacedSendOptions {
    pub wait: WaitStrategy,
    /// Tolerancia de adelanto: al despertar se envía todo lo que vence antes
    /// de `ahora + quantum`. Acota el número de esperas por frame.
    pub quantum: Duration,
}

impl Default for PacedSendOptions {
    fn default() -> Self {
        PacedSendOptions {
            wait: WaitStrategy::default(),
            quantum: Duration::from_micros(250),
        }
    }
}

/// Resultado del envío de un frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacedSendReport {
    /// Instante en que se entregó a `send` el último datagrama.
    pub last_sent_at: Option<Instant>,
    pub sent: usize,
    pub failed: usize,
    /// Número de esperas (despertares) usadas.
    pub waits: usize,
    /// Mayor retraso de un datagrama respecto a su instante planificado.
    pub max_late: Duration,
}

/// Envía `datagrams` siguiendo `schedule` (de [`Pacer::schedule`]). `send`
/// devuelve `false` si el datagrama no se pudo encolar (se cuenta y sigue).
///
/// Con las estrategias de hilo el envío ocurre en `spawn_blocking`; `send`
/// debe ser síncrono (p. ej. `Connection::send_datagram`).
pub async fn send_paced<F>(
    datagrams: Vec<Bytes>,
    schedule: Vec<Instant>,
    opts: PacedSendOptions,
    send: F,
) -> PacedSendReport
where
    F: FnMut(Bytes) -> bool + Send + 'static,
{
    assert_eq!(datagrams.len(), schedule.len(), "un instante por datagrama");
    match opts.wait {
        WaitStrategy::HighResTimer | WaitStrategy::ThreadSleep => {
            let high_res = opts.wait == WaitStrategy::HighResTimer;
            let q = opts.quantum;
            tokio::task::spawn_blocking(move || {
                let mut run = Run::new(datagrams, schedule, q, send);
                while let Some(wait) = run.step() {
                    if high_res {
                        crate::os::high_res_sleep(wait);
                    } else {
                        std::thread::sleep(wait);
                    }
                }
                run.report
            })
            .await
            .unwrap_or_default()
        }
        WaitStrategy::YieldSpin => {
            let mut run = Run::new(datagrams, schedule, opts.quantum, send);
            while let Some(wait) = run.step() {
                let until = Instant::now() + wait;
                while Instant::now() < until {
                    tokio::task::yield_now().await;
                }
            }
            run.report
        }
        WaitStrategy::TokioSleep => {
            let mut run = Run::new(datagrams, schedule, opts.quantum, send);
            while let Some(wait) = run.step() {
                tokio::time::sleep(wait).await;
            }
            run.report
        }
    }
}

/// Estado del envío de un frame, común a todas las estrategias.
struct Run<F> {
    datagrams: std::vec::IntoIter<Bytes>,
    schedule: Vec<Instant>,
    next: usize,
    quantum: Duration,
    send: F,
    report: PacedSendReport,
}

impl<F: FnMut(Bytes) -> bool> Run<F> {
    fn new(datagrams: Vec<Bytes>, schedule: Vec<Instant>, quantum: Duration, send: F) -> Self {
        Run {
            datagrams: datagrams.into_iter(),
            schedule,
            next: 0,
            quantum,
            send,
            report: PacedSendReport::default(),
        }
    }

    /// Envía la micro-ráfaga que vence ahora y devuelve cuánto esperar hasta
    /// la siguiente, o `None` si se terminó.
    fn step(&mut self) -> Option<Duration> {
        if self.next >= self.schedule.len() {
            return None;
        }
        let now = Instant::now();
        let end = burst_end(&self.schedule, self.next, now, self.quantum);
        for i in self.next..end {
            let late = now.saturating_duration_since(self.schedule[i]);
            self.report.max_late = self.report.max_late.max(late);
            let d = self.datagrams.next().expect("un datagrama por instante");
            if (self.send)(d) {
                self.report.sent += 1;
            } else {
                self.report.failed += 1;
            }
        }
        if end > self.next {
            self.report.last_sent_at = Some(Instant::now());
        }
        self.next = end;
        let due = *self.schedule.get(self.next)?;
        self.report.waits += 1;
        Some(due.saturating_duration_since(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn bucket_spaces_sends_at_rate() {
        let t0 = Instant::now();
        // 1000 B/s, capacidad 100 B, bucket lleno al principio.
        let mut b = TokenBucket::new(1000.0, 100, t0);
        assert_eq!(b.reserve(100, t0), t0);
        assert_eq!(b.reserve(100, t0), t0 + ms(100));
        assert_eq!(b.reserve(50, t0), t0 + ms(150));
        // Tras un rato inactivo el bucket se rellena, pero nunca por encima
        // de su capacidad.
        let later = t0 + Duration::from_secs(10);
        assert_eq!(b.reserve(100, later), later);
        assert_eq!(b.reserve(100, later), later + ms(100));
    }

    #[test]
    fn oversized_send_creates_debt() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1000.0, 100, t0);
        assert_eq!(b.reserve(300, t0), t0);
        // Deuda de 200 B + 100 B nuevos → 300 ms.
        assert_eq!(b.reserve(100, t0), t0 + ms(300));
    }

    #[test]
    fn spread_finishes_within_window() {
        let t0 = Instant::now();
        let mut p = Pacer::new(PacerConfig {
            mode: PacingMode::Spread { fraction: 0.6 },
            burst_bytes: 2400,
        });
        let sizes = vec![1200usize; 100];
        let interval = Duration::from_micros(16_667);
        let s = p.schedule(&sizes, interval, t0);
        assert_eq!(s.len(), 100);
        assert!(s.windows(2).all(|w| w[0] <= w[1]));
        // Los dos primeros (2400 B de burst) salen al instante.
        assert_eq!(s[0], t0);
        assert_eq!(s[1], t0);
        assert!(s[2] > t0);
        let last = *s.last().unwrap() - t0;
        let window = interval.mul_f64(0.6);
        assert!(last <= window, "{last:?} > {window:?}");
        assert!(
            last >= window.mul_f64(0.97),
            "{last:?} muy por debajo de {window:?}"
        );
        // Espaciado uniforme tras el burst.
        let gap = s[3] - s[2];
        assert!(s[2..]
            .windows(2)
            .all(|w| (w[1] - w[0]).abs_diff(gap) < Duration::from_micros(2)));
    }

    #[test]
    fn next_frame_waits_for_unfinished_previous_frame() {
        let t0 = Instant::now();
        let mut p = Pacer::new(PacerConfig {
            mode: PacingMode::Rate {
                bytes_per_sec: 1_200_000.0,
            },
            burst_bytes: 1200,
        });
        let interval = ms(10);
        // 20 datagramas a 1 ms cada uno → el frame dura 19 ms > intervalo.
        let a = p.schedule(&[1200; 20], interval, t0);
        assert_eq!(*a.last().unwrap(), t0 + ms(19));
        let b = p.schedule(&[1200; 2], interval, t0 + interval);
        assert_eq!(b[0], t0 + ms(20));
        assert_eq!(b[1], t0 + ms(21));
    }

    #[test]
    fn off_sends_everything_now() {
        let t0 = Instant::now();
        let mut p = Pacer::new(PacerConfig {
            mode: PacingMode::Off,
            burst_bytes: 0,
        });
        assert_eq!(p.schedule(&[1200; 5], ms(16), t0), vec![t0; 5]);
    }

    #[test]
    fn small_frame_within_burst_is_not_delayed() {
        let t0 = Instant::now();
        let mut p = Pacer::new(PacerConfig::default());
        assert_eq!(p.schedule(&[1000, 500], ms(16), t0), vec![t0; 2]);
    }

    #[test]
    fn burst_end_groups_by_quantum() {
        let t0 = Instant::now();
        let s: Vec<Instant> = (0..10)
            .map(|i| t0 + Duration::from_micros(100 * i))
            .collect();
        assert_eq!(burst_end(&s, 0, t0, Duration::from_micros(250)), 3);
        assert_eq!(
            burst_end(&s, 3, t0 + Duration::from_micros(300), Duration::ZERO),
            4
        );
        assert_eq!(burst_end(&s, 9, t0 + ms(5), Duration::ZERO), 10);
    }

    #[test]
    fn parse_wait_strategy() {
        assert_eq!("highres".parse(), Ok(WaitStrategy::HighResTimer));
        assert_eq!("yield".parse(), Ok(WaitStrategy::YieldSpin));
        assert!("x".parse::<WaitStrategy>().is_err());
    }

    async fn check_strategy(wait: WaitStrategy) {
        let t0 = Instant::now() + ms(1);
        let schedule: Vec<Instant> = (0..20)
            .map(|i| t0 + Duration::from_micros(500 * i))
            .collect();
        let dgs: Vec<Bytes> = (0..20u8).map(|i| Bytes::from(vec![i])).collect();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = log.clone();
        let opts = PacedSendOptions {
            wait,
            quantum: Duration::from_micros(100),
        };
        let r = send_paced(dgs, schedule.clone(), opts, move |d| {
            l.lock().unwrap().push((d[0], Instant::now()));
            true
        })
        .await;
        assert_eq!(r.sent, 20);
        let log = log.lock().unwrap();
        // Orden preservado y nunca antes de `instante - quantum`.
        for (i, (b, at)) in log.iter().enumerate() {
            assert_eq!(usize::from(*b), i);
            assert!(*at + opts.quantum >= schedule[i]);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_paced_respects_order_and_schedule() {
        for w in [
            WaitStrategy::HighResTimer,
            WaitStrategy::ThreadSleep,
            WaitStrategy::YieldSpin,
            WaitStrategy::TokioSleep,
        ] {
            check_strategy(w).await;
        }
    }
}
