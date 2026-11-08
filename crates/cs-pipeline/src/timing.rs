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

/// Coalescencia de `RequestKeyframe`: como máximo un IDR forzado cada
/// `min_interval` (contrato: 100 ms). Una petición dentro de la ventana no se
/// pierde: queda diferida hasta que la ventana se cumple.
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

    /// Llega una petición. `true` = forzar el IDR ya.
    pub fn request(&mut self, now: Instant) -> bool {
        if self.ready(now) {
            self.last_forced = Some(now);
            self.deferred = false;
            true
        } else {
            self.deferred = true;
            false
        }
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
}
