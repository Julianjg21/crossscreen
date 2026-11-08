//! Piezas puras de temporización: `pts` del contrato y coalescencia de
//! peticiones de IDR.

use std::time::{Duration, Instant};

/// Genera los `pts` de los frames.
///
/// - Hacia el codificador: µs de QPC completos (u64), **estrictamente
///   crecientes** (el MFT empareja salida y entrada por `pts`).
/// - En el cable (`FrameMeta.pts_us`): los mismos µs módulo 2³², como pide el
///   contrato, para que un receptor en el mismo equipo calcule la latencia
///   captura → presentación con su propio QPC.
#[derive(Debug, Default, Clone)]
pub struct PtsClock {
    last: u64,
}

impl PtsClock {
    /// `present_us` es el `LastPresentTime` de DXGI (0 = desconocido) y
    /// `acquired_us` el QPC al adquirir. Prefiere el primero, salvo que sea
    /// más antiguo que [`MAX_PRESENT_AGE_US`]: el primer frame tras crear la
    /// duplicación trae la última presentación de DWM, que puede ser de hace
    /// segundos con el escritorio estático (medido: 754 ms), y daría una
    /// "latencia" que no es del pipeline.
    pub fn next(&mut self, present_us: u64, acquired_us: u64) -> u64 {
        let fresh = present_us != 0 && acquired_us.saturating_sub(present_us) <= MAX_PRESENT_AGE_US;
        let t = if fresh { present_us } else { acquired_us };
        let t = t.max(self.last + 1);
        self.last = t;
        t
    }
}

/// Antigüedad máxima aceptada de `LastPresentTime` respecto a la adquisición.
pub const MAX_PRESENT_AGE_US: u64 = 100_000;

/// `pts` del cable: QPC en µs módulo 2³².
pub fn wire_pts(pts_us: u64) -> u32 {
    pts_us as u32
}

/// Tope del RTT considerado al ignorar peticiones repetidas (una estimación
/// inicial de QUIC antes de que haya tráfico puede acercarse a 1 s).
pub const MAX_RTT_IGNORE: Duration = Duration::from_millis(500);

/// Decisión ante una petición `RequestKeyframe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeDecision {
    /// Forzar un IDR inmediatamente.
    ForceNow,
    /// Dentro del intervalo mínimo del contrato (pero después de ~1 RTT):
    /// queda diferido hasta cumplir `min_interval`.
    Deferred,
    /// Dentro de ~1 RTT del último IDR forzado: el receptor aún no podía
    /// haber recibido ese IDR cuando envió la petición, así que se ignora.
    Ignored,
}

/// Coalescencia de `RequestKeyframe`: como máximo un IDR forzado cada
/// `min_interval` (contrato: 100 ms).
///
/// Además, con [`KeyframeCoalescer::request_with_rtt`], las peticiones que
/// llegan dentro de `~1 RTT` (acotado a [`MAX_RTT_IGNORE`]) del último IDR
/// forzado se **ignoran** en lugar de diferirse: se enviaron antes de que el
/// IDR pudiera llegar al receptor, y diferirlas generaba un segundo IDR
/// redundante 100 ms después.
#[derive(Debug, Clone)]
pub struct KeyframeCoalescer {
    min_interval: Duration,
    last_forced: Option<Instant>,
    deferred: bool,
}

impl KeyframeCoalescer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_forced: None,
            deferred: false,
        }
    }

    /// Anota que se acaba de forzar un IDR (por ejemplo, el inicial al
    /// arrancar el flujo).
    pub fn note_forced(&mut self, now: Instant) {
        self.last_forced = Some(now);
        self.deferred = false;
    }

    /// Llega una petición (sin filtro de RTT). `true` = forzar el IDR ya.
    pub fn request(&mut self, now: Instant) -> bool {
        self.request_with_rtt(now, Duration::ZERO) == KeyframeDecision::ForceNow
    }

    /// Llega una petición con el RTT actual de la conexión.
    pub fn request_with_rtt(&mut self, now: Instant, rtt: Duration) -> KeyframeDecision {
        let ignore_window = rtt.min(MAX_RTT_IGNORE);
        if let Some(last) = self.last_forced {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < ignore_window {
                return KeyframeDecision::Ignored;
            }
            if elapsed < self.min_interval {
                self.deferred = true;
                return KeyframeDecision::Deferred;
            }
        }
        self.last_forced = Some(now);
        self.deferred = false;
        KeyframeDecision::ForceNow
    }

    /// Instante en que vence la petición diferida, si la hay.
    pub fn deferred_until(&self) -> Option<Instant> {
        self.deferred
            .then(|| self.last_forced.map(|t| t + self.min_interval))
            .flatten()
    }

    /// Comprueba la petición diferida. `true` = forzar el IDR ya.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.deferred && self.ready(now) {
            self.last_forced = Some(now);
            self.deferred = false;
            true
        } else {
            false
        }
    }

    fn ready(&self, now: Instant) -> bool {
        self.last_forced
            .is_none_or(|t| now.saturating_duration_since(t) >= self.min_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_prefers_present_time_and_is_monotonic() {
        let mut c = PtsClock::default();
        assert_eq!(c.next(1000, 1500), 1000);
        assert_eq!(c.next(0, 2000), 2000);
        // Un LastPresentTime repetido o anterior no rompe la monotonía.
        assert_eq!(c.next(2000, 2100), 2001);
        assert_eq!(c.next(1500, 2100), 2002);
        // Presentación demasiado antigua (primer frame tras la duplicación).
        assert_eq!(c.next(10_000, 1_000_000), 1_000_000);
    }

    #[test]
    fn wire_pts_wraps_mod_2_32() {
        assert_eq!(wire_pts(5), 5);
        assert_eq!(wire_pts((1u64 << 32) + 7), 7);
    }

    #[test]
    fn coalescer_limits_and_defers() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut k = KeyframeCoalescer::new(ms(100));
        assert!(k.request(t0));
        assert!(!k.request(t0 + ms(10)));
        assert!(!k.request(t0 + ms(50)));
        assert_eq!(k.deferred_until(), Some(t0 + ms(100)));
        assert!(!k.poll(t0 + ms(99)));
        assert!(k.poll(t0 + ms(100)));
        // Las peticiones intermedias se fundieron en un solo IDR.
        assert!(!k.poll(t0 + ms(300)));
        assert_eq!(k.deferred_until(), None);
        assert!(k.request(t0 + ms(300)));
    }

    #[test]
    fn coalescer_ignores_within_rtt_and_defers_after_rtt() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let rtt = ms(30);
        let mut k = KeyframeCoalescer::new(ms(100));
        assert_eq!(k.request_with_rtt(t0, rtt), KeyframeDecision::ForceNow);
        // Dentro de 1 RTT (30 ms): ignorada, NO queda diferida.
        assert_eq!(
            k.request_with_rtt(t0 + ms(10), rtt),
            KeyframeDecision::Ignored
        );
        assert_eq!(
            k.request_with_rtt(t0 + ms(29), rtt),
            KeyframeDecision::Ignored
        );
        assert_eq!(k.deferred_until(), None);
        assert!(!k.poll(t0 + ms(100)));
        // Entre 1 RTT (30 ms) y 100 ms: sí se difiere a t0 + 100 ms.
        let mut k = KeyframeCoalescer::new(ms(100));
        assert_eq!(k.request_with_rtt(t0, rtt), KeyframeDecision::ForceNow);
        assert_eq!(
            k.request_with_rtt(t0 + ms(40), rtt),
            KeyframeDecision::Deferred
        );
        assert_eq!(k.deferred_until(), Some(t0 + ms(100)));
        assert!(k.poll(t0 + ms(100)));
        // Si el RTT supera 100 ms (p. ej. 250 ms), ignora hasta 250 ms y
        // después fuerza al instante.
        let mut k = KeyframeCoalescer::new(ms(100));
        k.note_forced(t0);
        assert_eq!(
            k.request_with_rtt(t0 + ms(200), ms(250)),
            KeyframeDecision::Ignored
        );
        assert_eq!(
            k.request_with_rtt(t0 + ms(260), ms(250)),
            KeyframeDecision::ForceNow
        );
    }
}
