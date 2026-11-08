//! Acumulador sencillo de latencias para informar p50/p95/máx.

use std::time::Duration;

/// Muestras de latencia en microsegundos.
#[derive(Debug, Default, Clone)]
pub struct LatencyStats {
    samples_us: Vec<u64>,
}

/// Resumen de un [`LatencyStats`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySummary {
    /// Número de muestras.
    pub count: usize,
    /// Mediana en milisegundos.
    pub p50_ms: f64,
    /// Percentil 95 en milisegundos.
    pub p95_ms: f64,
    /// Percentil 99 en milisegundos.
    pub p99_ms: f64,
    /// Máximo en milisegundos.
    pub max_ms: f64,
    /// Media en milisegundos.
    pub mean_ms: f64,
}

impl LatencyStats {
    /// Crea un acumulador vacío.
    pub fn new() -> Self {
        Self::default()
    }

    /// Añade una muestra.
    pub fn record(&mut self, d: Duration) {
        self.samples_us.push(d.as_micros() as u64);
    }

    /// Número de muestras.
    pub fn len(&self) -> usize {
        self.samples_us.len()
    }

    /// ¿Sin muestras?
    pub fn is_empty(&self) -> bool {
        self.samples_us.is_empty()
    }

    /// Calcula el resumen (método del rango más cercano). `None` si no hay
    /// muestras.
    pub fn summary(&self) -> Option<LatencySummary> {
        if self.samples_us.is_empty() {
            return None;
        }
        let mut s = self.samples_us.clone();
        s.sort_unstable();
        let pct = |p: f64| {
            let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
            s[rank.clamp(1, s.len()) - 1] as f64 / 1000.0
        };
        let sum: u64 = s.iter().sum();
        Some(LatencySummary {
            count: s.len(),
            p50_ms: pct(50.0),
            p95_ms: pct(95.0),
            p99_ms: pct(99.0),
            max_ms: *s.last().unwrap_or(&0) as f64 / 1000.0,
            mean_ms: sum as f64 / s.len() as f64 / 1000.0,
        })
    }
}

impl std::fmt::Display for LatencySummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} p50={:.3} ms p95={:.3} ms p99={:.3} ms max={:.3} ms media={:.3} ms",
            self.count, self.p50_ms, self.p95_ms, self.p99_ms, self.max_ms, self.mean_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        let mut st = LatencyStats::new();
        assert!(st.summary().is_none());
        for ms in 1..=100u64 {
            st.record(Duration::from_millis(ms));
        }
        let s = st.summary().unwrap();
        assert_eq!(s.count, 100);
        assert_eq!(s.p50_ms, 50.0);
        assert_eq!(s.p95_ms, 95.0);
        assert_eq!(s.max_ms, 100.0);
        assert!((s.mean_ms - 50.5).abs() < 1e-9);
    }
}
