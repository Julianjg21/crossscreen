//! Regulador de tasa del emisor guiado por el retardo de la red.
//!
//! Problema medido (relé UDP que imita un Wi-Fi malo: 1,2 Mbit/s, cola de
//! 512 KiB y cortes de 250 ms cada 2 s): si el emisor manda más de lo que
//! cabe por el enlace, el vídeo se encola en el router (*bufferbloat*) y
//! cada frame llega más tarde que el anterior: **2,2 s de retraso añadido
//! de mediana y 4,2 s de máximo**. QUIC no lo evita: ni Cubic ni BBR frenaron
//! los datagramas en esa prueba.
//!
//! Solución (la idea de GCC en WebRTC, simplificada): la cola de la red se
//! ve en el RTT. `retardo de cola = RTT suavizado − RTT mínimo reciente`.
//! - Si pasa de [`QUEUE_HIGH`], la tasa pasa a 0,85 × lo que **de verdad**
//!   se estaba enviando (como GCC con la tasa recibida; como mucho una vez
//!   cada [`DECREASE_HOLD`], el tiempo que tarda en notarse). Partir de la
//!   tasa teórica no sirve: con 20 Mbit/s configurados y un vídeo de
//!   1 Mbit/s, cada bajada no cambiaba nada durante segundos.
//! - Si está por debajo de [`QUEUE_LOW`], sube un 5 % cada
//!   [`INCREASE_EVERY`] hasta la tasa configurada.
//! - La tasa se aplica con un cubo de fichas antes de codificar: si no hay
//!   saldo, no se codifica (bajan los fps); el codificador además sube el QP
//!   (frames más pequeños) con la tasa nueva.
//!
//! Lógica pura (sin red ni relojes propios) para poder probarla.

use std::time::{Duration, Instant};

/// Retardo de cola a partir del cual se baja la tasa.
pub const QUEUE_HIGH: Duration = Duration::from_millis(40);
/// Retardo de cola por debajo del cual se puede subir.
pub const QUEUE_LOW: Duration = Duration::from_millis(15);
/// Espera mínima entre dos bajadas (que la anterior surta efecto).
pub const DECREASE_HOLD: Duration = Duration::from_millis(300);
/// Cadencia de las subidas.
pub const INCREASE_EVERY: Duration = Duration::from_millis(100);
/// Ventana del RTT mínimo (si la ruta cambia, el mínimo viejo caduca).
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
/// Tasa mínima: por debajo, mejor pocos fps que imagen ilegible.
pub const MIN_RATE_KBPS: u32 = 300;
/// Saldo máximo acumulable del cubo, en tiempo a la tasa actual: deja pasar
/// un IDR sin esperar, pero no ráfagas largas tras una pausa.
const BUCKET_DEPTH: Duration = Duration::from_millis(250);

/// Estado del regulador.
#[derive(Debug, Clone)]
pub struct RateGovernor {
    max_kbps: u32,
    rate_kbps: f64,
    /// Saldo del cubo en bytes (negativo = deuda).
    tokens: f64,
    last_refill: Instant,
    last_decrease: Option<Instant>,
    last_increase: Instant,
    /// (instante, RTT) del mínimo actual y del candidato a sustituirlo.
    min_rtt: Option<(Instant, Duration)>,
    queue_delay: Duration,
    decreases: u64,
    lowest_kbps: f64,
    /// Tasa real de envío (media móvil, kbps) y su ventana en curso.
    sent_kbps: f64,
    window_bytes: f64,
    window_start: Instant,
}

/// Duración de cada ventana de medida de la tasa real.
const SEND_RATE_WINDOW: Duration = Duration::from_millis(200);

impl RateGovernor {
    pub fn new(max_kbps: u32, now: Instant) -> Self {
        let max_kbps = max_kbps.max(MIN_RATE_KBPS);
        Self {
            max_kbps,
            rate_kbps: f64::from(max_kbps),
            tokens: 0.0,
            last_refill: now,
            last_decrease: None,
            last_increase: now,
            min_rtt: None,
            queue_delay: Duration::ZERO,
            decreases: 0,
            lowest_kbps: f64::from(max_kbps),
            sent_kbps: 0.0,
            window_bytes: 0.0,
            window_start: now,
        }
    }

    fn measure_send_rate(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.window_start);
        if dt < SEND_RATE_WINDOW {
            return;
        }
        let inst = self.window_bytes * 8.0 / 1000.0 / dt.as_secs_f64();
        self.sent_kbps = if self.sent_kbps == 0.0 {
            inst
        } else {
            self.sent_kbps * 0.6 + inst * 0.4
        };
        self.window_bytes = 0.0;
        self.window_start = now;
    }

    fn bytes_per_sec(&self) -> f64 {
        self.rate_kbps * 1000.0 / 8.0
    }

    /// Nueva muestra de RTT (el suavizado de QUIC); ajusta la tasa.
    pub fn on_rtt(&mut self, now: Instant, rtt: Duration) {
        self.refill(now);
        self.measure_send_rate(now);
        // RTT mínimo en la ventana: se renueva si caduca o si hay uno menor.
        match self.min_rtt {
            Some((at, min)) if rtt >= min && now.duration_since(at) < MIN_RTT_WINDOW => {}
            _ => self.min_rtt = Some((now, rtt)),
        }
        let min = self.min_rtt.map_or(rtt, |(_, m)| m);
        self.queue_delay = rtt.saturating_sub(min);
        if self.queue_delay > QUEUE_HIGH {
            let can = self
                .last_decrease
                .is_none_or(|t| now.duration_since(t) >= DECREASE_HOLD);
            if can {
                let base = if self.sent_kbps > 0.0 {
                    self.rate_kbps.min(self.sent_kbps)
                } else {
                    self.rate_kbps
                };
                self.rate_kbps = (base * 0.85).max(f64::from(MIN_RATE_KBPS));
                self.lowest_kbps = self.lowest_kbps.min(self.rate_kbps);
                self.last_decrease = Some(now);
                self.decreases += 1;
                // Lo ahorrado no debe gastarse de golpe con la red llena.
                self.tokens = self.tokens.min(0.0);
            }
            self.last_increase = now;
        } else if self.queue_delay < QUEUE_LOW
            && now.duration_since(self.last_increase) >= INCREASE_EVERY
        {
            let step = (self.rate_kbps * 0.05).max(f64::from(self.max_kbps) * 0.01);
            self.rate_kbps = (self.rate_kbps + step).min(f64::from(self.max_kbps));
            self.last_increase = now;
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        let cap = self.bytes_per_sec() * BUCKET_DEPTH.as_secs_f64();
        self.tokens = (self.tokens + dt * self.bytes_per_sec()).min(cap);
    }

    /// ¿Se puede codificar otro frame ya? (sin deuda en el cubo)
    pub fn may_send(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= 0.0
    }

    /// Se envió un frame de `bytes`.
    pub fn spent(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
        self.window_bytes += bytes as f64;
    }

    /// Tasa actual.
    pub fn rate_kbps(&self) -> u32 {
        self.rate_kbps.round() as u32
    }

    pub fn queue_delay(&self) -> Duration {
        self.queue_delay
    }

    /// Veces que se bajó la tasa y la más baja alcanzada.
    pub fn summary(&self) -> (u64, u32) {
        (self.decreases, self.lowest_kbps.round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn stays_at_max_on_a_clean_link() {
        let t0 = Instant::now();
        let mut g = RateGovernor::new(8000, t0);
        for i in 0..200 {
            g.on_rtt(t0 + i * 10 * MS, 3 * MS + (i % 3) * MS);
        }
        assert_eq!(g.rate_kbps(), 8000);
        assert_eq!(g.summary().0, 0);
    }

    #[test]
    fn backs_off_when_the_queue_grows_and_recovers_after() {
        let t0 = Instant::now();
        let mut g = RateGovernor::new(8000, t0);
        g.on_rtt(t0, 4 * MS);
        // Cola creciendo: el RTT sube 200 ms en 1 s.
        let mut t = t0;
        for i in 1..=100u32 {
            t = t0 + i * 10 * MS;
            g.on_rtt(t, 4 * MS + i * 2 * MS);
        }
        // La cola pasa de 40 ms a los ~190 ms; bajadas a los 190, 490 y
        // 790 ms (una cada DECREASE_HOLD). Sin envíos medidos se parte de
        // la tasa actual: 8000 · 0,85³ ≈ 4913.
        assert_eq!(g.summary().0, 3, "{:?}", g.summary());
        assert_eq!(g.rate_kbps(), 4913);
        // La cola se vacía: vuelve a subir poco a poco hasta el máximo.
        for i in 1..=600u32 {
            g.on_rtt(t + i * 10 * MS, 4 * MS);
        }
        assert_eq!(g.rate_kbps(), 8000);
    }

    /// Con 20 Mbit/s de tope y un vídeo real de ~1 Mbit/s, la primera
    /// bajada debe quedar por debajo de lo enviado, no en 15 Mbit/s.
    #[test]
    fn first_decrease_starts_from_the_real_send_rate() {
        let t0 = Instant::now();
        let mut g = RateGovernor::new(20_000, t0);
        // 1 s enviando 2 kB cada 16 ms (≈ 1 Mbit/s) con la red limpia.
        let mut t = t0;
        for i in 0..60u32 {
            t = t0 + i * Duration::from_micros(16_667);
            g.spent(2_000);
            g.on_rtt(t, 3 * MS);
        }
        assert_eq!(g.rate_kbps(), 20_000);
        // Cola: el RTT salta 100 ms.
        g.on_rtt(t + 20 * MS, 103 * MS);
        let r = g.rate_kbps();
        assert!((700..=950).contains(&r), "tras la primera bajada: {r}");
    }

    #[test]
    fn never_goes_below_the_floor() {
        let t0 = Instant::now();
        let mut g = RateGovernor::new(2000, t0);
        g.on_rtt(t0, 2 * MS);
        // Cola llena durante 9 s (dentro de la ventana del RTT mínimo).
        for i in 1..=90u32 {
            g.on_rtt(t0 + i * 100 * MS, 500 * MS);
        }
        assert_eq!(g.rate_kbps(), MIN_RATE_KBPS);
    }

    #[test]
    fn token_bucket_limits_the_frame_rate() {
        let t0 = Instant::now();
        // 800 kbps = 100 kB/s; frames de 10 kB a 60 fps → ~10 fps.
        let mut g = RateGovernor::new(800, t0);
        let mut sent = 0;
        for i in 0..600u32 {
            let now = t0 + i * Duration::from_micros(16_667);
            if g.may_send(now) {
                g.spent(10_000);
                sent += 1;
            }
        }
        // 10 s a 100 kB/s = 100 frames (+ el saldo inicial del cubo).
        assert!((95..=105).contains(&sent), "{sent}");
    }

    #[test]
    fn old_minimum_expires() {
        let t0 = Instant::now();
        let mut g = RateGovernor::new(8000, t0);
        g.on_rtt(t0, 2 * MS);
        // La ruta cambia: RTT base de 30 ms. Tras la ventana, deja de
        // contar como cola.
        let later = t0 + MIN_RTT_WINDOW + 10 * MS;
        g.on_rtt(later, 30 * MS);
        assert!(g.queue_delay() < QUEUE_LOW, "{:?}", g.queue_delay());
    }
}
