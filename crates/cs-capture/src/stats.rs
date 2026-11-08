//! Estadística simple de muestras de latencia (percentiles por rango más cercano).

/// Conjunto de muestras (p. ej. microsegundos).
#[derive(Debug, Clone, Default)]
pub struct Samples {
    values: Vec<f64>,
    sorted: bool,
}

impl Samples {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, v: f64) {
        self.values.push(v);
        self.sorted = false;
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn sort(&mut self) {
        if !self.sorted {
            self.values.sort_by(|a, b| a.total_cmp(b));
            self.sorted = true;
        }
    }

    /// Percentil `p` (0..=100) por rango más cercano; `None` si no hay muestras.
    pub fn percentile(&mut self, p: f64) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        self.sort();
        let n = self.values.len();
        let rank = ((p.clamp(0.0, 100.0) / 100.0) * n as f64).ceil() as usize;
        Some(self.values[rank.clamp(1, n) - 1])
    }

    pub fn max(&mut self) -> Option<f64> {
        self.percentile(100.0)
    }

    pub fn min(&mut self) -> Option<f64> {
        self.percentile(0.0)
    }

    pub fn mean(&self) -> Option<f64> {
        if self.values.is_empty() {
            None
        } else {
            Some(self.values.iter().sum::<f64>() / self.values.len() as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_percentiles() {
        let mut s = Samples::new();
        assert_eq!(s.percentile(50.0), None);
        assert_eq!(s.mean(), None);
        assert!(s.is_empty());
    }

    #[test]
    fn nearest_rank_percentiles() {
        let mut s = Samples::new();
        for v in (1..=100).rev() {
            s.push(v as f64);
        }
        assert_eq!(s.percentile(50.0), Some(50.0));
        assert_eq!(s.percentile(95.0), Some(95.0));
        assert_eq!(s.percentile(99.5), Some(100.0));
        assert_eq!(s.max(), Some(100.0));
        assert_eq!(s.min(), Some(1.0));
        assert_eq!(s.mean(), Some(50.5));
        // Añadir tras ordenar vuelve a ordenar.
        s.push(0.5);
        assert_eq!(s.min(), Some(0.5));
    }

    #[test]
    fn single_sample() {
        let mut s = Samples::new();
        s.push(7.0);
        assert_eq!(s.percentile(0.0), Some(7.0));
        assert_eq!(s.percentile(95.0), Some(7.0));
    }
}
