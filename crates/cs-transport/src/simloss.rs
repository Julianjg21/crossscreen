//! **Solo pruebas.** Inyector de pérdidas determinista para reproducir en
//! loopback pérdidas en ráfaga como las de una Wi-Fi.
//!
//! Modelo de Gilbert-Elliott por datagrama, con dos estados (bueno / malo):
//!
//! - `p`: probabilidad de pasar de bueno a malo tras cada datagrama.
//! - `r`: probabilidad de pasar de malo a bueno.
//! - `h`: probabilidad de **perder** un datagrama en el estado malo.
//! - `k`: probabilidad de perderlo en el estado bueno (por defecto 0).
//!
//! Ojo: `h` es aquí la probabilidad de pérdida (en la notación de `netem`,
//! `1-h`). Pérdida media = `π_B·h + (1−π_B)·k` con `π_B = p / (p + r)`; la
//! duración media de una estancia en el estado malo es `1/r` datagramas.
//!
//! También admite `bernoulli:p=0.01` (pérdidas independientes). El
//! generador es SplitMix64 con semilla explícita: la misma semilla da la
//! misma secuencia de pérdidas en cualquier equipo.

use std::str::FromStr;

/// Modelo de pérdidas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LossModel {
    Bernoulli { p: f64 },
    Gilbert { p: f64, r: f64, h: f64, k: f64 },
}

impl LossModel {
    /// Pérdida media teórica (estado estacionario).
    pub fn mean_loss(&self) -> f64 {
        match *self {
            LossModel::Bernoulli { p } => p,
            LossModel::Gilbert { p, r, h, k } => {
                if p + r <= 0.0 {
                    k
                } else {
                    let pb = p / (p + r);
                    pb * h + (1.0 - pb) * k
                }
            }
        }
    }
}

impl std::fmt::Display for LossModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LossModel::Bernoulli { p } => write!(f, "bernoulli:p={p}"),
            LossModel::Gilbert { p, r, h, k } => write!(f, "gilbert:p={p},r={r},h={h},k={k}"),
        }
    }
}

impl FromStr for LossModel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, params) = s
            .split_once(':')
            .ok_or_else(|| format!("modelo de pérdidas sin parámetros: {s}"))?;
        let mut get = std::collections::HashMap::new();
        for kv in params.split(',').filter(|x| !x.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("parámetro sin '=': {kv}"))?;
            let v: f64 = v
                .trim()
                .parse()
                .map_err(|_| format!("valor no numérico en {kv}"))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{kv}: las probabilidades van de 0 a 1"));
            }
            get.insert(k.trim().to_string(), v);
        }
        let need = |k: &str| {
            get.get(k)
                .copied()
                .ok_or_else(|| format!("falta el parámetro {k} en {s}"))
        };
        let m = match kind {
            "bernoulli" => LossModel::Bernoulli { p: need("p")? },
            "gilbert" => LossModel::Gilbert {
                p: need("p")?,
                r: need("r")?,
                h: get.get("h").copied().unwrap_or(1.0),
                k: get.get("k").copied().unwrap_or(0.0),
            },
            _ => return Err(format!("modelo desconocido: {kind} (gilbert|bernoulli)")),
        };
        Ok(m)
    }
}

/// Generador SplitMix64 (determinista y portable).
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniforme en `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Inyector de pérdidas con estado.
#[derive(Debug, Clone)]
pub struct LossInjector {
    model: LossModel,
    bad: bool,
    rng: SplitMix64,
    pub seen: u64,
    pub dropped: u64,
    /// Ráfagas de pérdidas consecutivas y la más larga.
    pub bursts: u64,
    pub longest_burst: u64,
    run: u64,
}

impl LossInjector {
    pub fn new(model: LossModel, seed: u64) -> Self {
        LossInjector {
            model,
            bad: false,
            rng: SplitMix64(seed),
            seen: 0,
            dropped: 0,
            bursts: 0,
            longest_burst: 0,
            run: 0,
        }
    }

    pub fn model(&self) -> LossModel {
        self.model
    }

    /// `true` = descartar el siguiente datagrama.
    pub fn drop_next(&mut self) -> bool {
        self.seen += 1;
        let lost = match self.model {
            LossModel::Bernoulli { p } => self.rng.next_f64() < p,
            LossModel::Gilbert { p, r, h, k } => {
                let lost = self.rng.next_f64() < if self.bad { h } else { k };
                let flip = self.rng.next_f64() < if self.bad { r } else { p };
                if flip {
                    self.bad = !self.bad;
                }
                lost
            }
        };
        if lost {
            self.dropped += 1;
            if self.run == 0 {
                self.bursts += 1;
            }
            self.run += 1;
            self.longest_burst = self.longest_burst.max(self.run);
        } else {
            self.run = 0;
        }
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_models() {
        assert_eq!(
            "gilbert:p=0.015,r=0.25,h=0.8".parse::<LossModel>(),
            Ok(LossModel::Gilbert {
                p: 0.015,
                r: 0.25,
                h: 0.8,
                k: 0.0
            })
        );
        assert_eq!(
            "bernoulli:p=0.01".parse::<LossModel>(),
            Ok(LossModel::Bernoulli { p: 0.01 })
        );
        assert!("gilbert:p=0.1".parse::<LossModel>().is_err());
        assert!("gilbert:p=2,r=0.1".parse::<LossModel>().is_err());
        assert!("netem:p=0.1".parse::<LossModel>().is_err());
        assert!("gilbert".parse::<LossModel>().is_err());
    }

    #[test]
    fn deterministic_per_seed() {
        let m: LossModel = "gilbert:p=0.015,r=0.25,h=0.8".parse().unwrap();
        let run = |seed| {
            let mut i = LossInjector::new(m, seed);
            (0..10_000).map(|_| i.drop_next()).collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn gilbert_mean_loss_and_bursts() {
        let m: LossModel = "gilbert:p=0.015,r=0.25,h=0.8".parse().unwrap();
        let expected = m.mean_loss();
        assert!((expected - 0.0453).abs() < 1e-3, "{expected}");
        let mut i = LossInjector::new(m, 42);
        for _ in 0..400_000 {
            i.drop_next();
        }
        let got = i.dropped as f64 / i.seen as f64;
        assert!((got - expected).abs() < 0.005, "{got} vs {expected}");
        // Ráfagas: más de un datagrama por ráfaga de media.
        assert!(i.dropped as f64 / i.bursts as f64 > 2.0);
    }
}
