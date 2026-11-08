//! FEC adaptativo (sans-IO): ajusta la paridad de los P según `ReceiverStats`.
//!
//! El receptor informa cada ~1 s de contadores **acumulados** (`frames_lost`,
//! `frames_received`) y de una estimación de pérdida antes de FEC
//! (`pre_fec_loss_bp`, en partes por diez mil). La política es del estilo
//! AIMD pero al revés que en control de congestión: subir rápido cuando hay
//! problemas y bajar despacio cuando todo va bien.
//!
//! - **Frames perdidos tras FEC** en el intervalo → la paridad sube de forma
//!   multiplicativa (`× increase_factor`, como mínimo `+ increase_step`).
//! - **Suelo por pérdida previa a FEC**: la paridad nunca queda por debajo de
//!   `min + loss_multiplier × pérdida` (las ráfagas exigen bastante más
//!   paridad que la pérdida media).
//! - **Histéresis**: solo se baja `decrease_step` tras `clean_reports`
//!   informes seguidos sin frames perdidos (y con frames recibidos); el
//!   contador vuelve a cero con cada bajada o subida.
//!
//! Todo dentro de `[min, max]`.

/// Parámetros de la política.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveFecConfig {
    pub min: f32,
    pub max: f32,
    pub initial: f32,
    pub increase_factor: f32,
    pub increase_step: f32,
    pub decrease_step: f32,
    pub clean_reports: u32,
    pub loss_multiplier: f32,
}

impl Default for AdaptiveFecConfig {
    fn default() -> Self {
        AdaptiveFecConfig {
            min: 0.05,
            max: 0.5,
            initial: 0.15,
            increase_factor: 1.5,
            increase_step: 0.05,
            decrease_step: 0.02,
            clean_reports: 5,
            loss_multiplier: 3.0,
        }
    }
}

/// Motivo de un cambio de paridad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FecChangeReason {
    /// Frames perdidos tras FEC en el último intervalo.
    FramesLost(u32),
    /// La pérdida previa a FEC exige más paridad (suelo).
    PreFecLoss(u16),
    /// Varios informes seguidos sin pérdidas.
    Clean,
}

/// Cambio de paridad propuesto por [`AdaptiveFec::on_stats`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FecChange {
    pub from: f32,
    pub to: f32,
    pub reason: FecChangeReason,
}

/// Estado del FEC adaptativo.
#[derive(Debug, Clone)]
pub struct AdaptiveFec {
    cfg: AdaptiveFecConfig,
    ratio: f32,
    prev: Option<(u32, u32)>,
    clean: u32,
}

impl AdaptiveFec {
    pub fn new(cfg: AdaptiveFecConfig) -> Self {
        let ratio = cfg.initial.clamp(cfg.min, cfg.max);
        AdaptiveFec {
            cfg,
            ratio,
            prev: None,
            clean: 0,
        }
    }

    /// Paridad actual de los P.
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// Procesa un `ReceiverStats` (contadores acumulados). Devuelve el cambio
    /// si la paridad cambia.
    pub fn on_stats(
        &mut self,
        frames_received: u32,
        frames_lost: u32,
        pre_fec_loss_bp: u16,
    ) -> Option<FecChange> {
        let c = self.cfg;
        let (d_recv, d_lost) = match self.prev {
            // Contadores que retroceden: receptor reiniciado; tomar como base.
            Some((r, l)) if frames_received < r || frames_lost < l => (0, 0),
            Some((r, l)) => (frames_received - r, frames_lost - l),
            // Primer informe: lo acumulado hasta ahora cuenta como intervalo.
            None => (frames_received, frames_lost),
        };
        self.prev = Some((frames_received, frames_lost));

        let loss = f32::from(pre_fec_loss_bp.min(10_000)) / 10_000.0;
        let floor = (c.min + c.loss_multiplier * loss).clamp(c.min, c.max);
        let from = self.ratio;
        let (to, reason) = if d_lost > 0 {
            self.clean = 0;
            let up = (from * c.increase_factor).max(from + c.increase_step);
            (up.max(floor), FecChangeReason::FramesLost(d_lost))
        } else if from < floor {
            self.clean = 0;
            (floor, FecChangeReason::PreFecLoss(pre_fec_loss_bp))
        } else if d_recv > 0 {
            self.clean += 1;
            if self.clean >= c.clean_reports && from > floor {
                self.clean = 0;
                ((from - c.decrease_step).max(floor), FecChangeReason::Clean)
            } else {
                (from, FecChangeReason::Clean)
            }
        } else {
            (from, FecChangeReason::Clean)
        };
        let to = to.clamp(c.min, c.max);
        if (to - from).abs() < 1e-4 {
            return None;
        }
        self.ratio = to;
        Some(FecChange { from, to, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fec() -> AdaptiveFec {
        AdaptiveFec::new(AdaptiveFecConfig::default())
    }

    #[test]
    fn starts_at_initial_clamped() {
        assert!((fec().ratio() - 0.15).abs() < 1e-6);
        let a = AdaptiveFec::new(AdaptiveFecConfig {
            initial: 0.9,
            ..Default::default()
        });
        assert!((a.ratio() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn raises_fast_on_lost_frames_up_to_max() {
        let mut a = fec();
        let ch = a.on_stats(60, 2, 0).unwrap();
        assert_eq!(ch.reason, FecChangeReason::FramesLost(2));
        assert!((ch.to - 0.225).abs() < 1e-4, "{ch:?}");
        // Más pérdidas: sigue subiendo hasta 0,5 y ahí se queda.
        let mut lost = 2;
        let mut recv = 60;
        for _ in 0..10 {
            lost += 1;
            recv += 60;
            a.on_stats(recv, lost, 0);
        }
        assert!((a.ratio() - 0.5).abs() < 1e-6);
        assert_eq!(a.on_stats(recv + 60, lost + 1, 0), None);
    }

    #[test]
    fn lowers_slowly_with_hysteresis() {
        let mut a = fec();
        a.on_stats(60, 1, 0); // 0,225
        let mut recv = 60;
        let mut changes = Vec::new();
        for i in 0..20 {
            recv += 60;
            if let Some(c) = a.on_stats(recv, 1, 0) {
                changes.push((i, c.to));
            }
        }
        // Una bajada de 0,02 cada 5 informes limpios.
        assert_eq!(changes.len(), 4, "{changes:?}");
        assert_eq!(changes[0].0, 4);
        assert!((changes[0].1 - 0.205).abs() < 1e-4);
        assert!((a.ratio() - 0.145).abs() < 1e-4);
        // Nunca por debajo del mínimo.
        for _ in 0..200 {
            recv += 60;
            a.on_stats(recv, 1, 0);
        }
        assert!((a.ratio() - 0.05).abs() < 1e-6);
    }

    #[test]
    fn loss_resets_the_clean_counter() {
        let mut a = fec();
        let mut recv = 0;
        for _ in 0..4 {
            recv += 60;
            assert_eq!(a.on_stats(recv, 0, 0), None);
        }
        recv += 60;
        assert!(a.on_stats(recv, 1, 0).is_some()); // sube
        for _ in 0..4 {
            recv += 60;
            assert_eq!(a.on_stats(recv, 1, 0), None); // aún sin bajar
        }
        recv += 60;
        assert_eq!(
            a.on_stats(recv, 1, 0).map(|c| c.reason),
            Some(FecChangeReason::Clean)
        );
    }

    #[test]
    fn pre_fec_loss_sets_a_floor() {
        let mut a = fec();
        // 5 % de pérdida previa → suelo 0,05 + 3·0,05 = 0,20.
        let ch = a.on_stats(60, 0, 500).unwrap();
        assert_eq!(ch.reason, FecChangeReason::PreFecLoss(500));
        assert!((ch.to - 0.20).abs() < 1e-4);
        // Sin pérdidas pero con el mismo suelo: no baja de 0,20.
        let mut recv = 60;
        for _ in 0..30 {
            recv += 60;
            a.on_stats(recv, 0, 500);
        }
        assert!((a.ratio() - 0.20).abs() < 1e-4);
    }

    #[test]
    fn idle_intervals_do_not_count_as_clean() {
        let mut a = fec();
        a.on_stats(60, 1, 0);
        for _ in 0..20 {
            assert_eq!(a.on_stats(60, 1, 0), None);
        }
    }

    #[test]
    fn counter_reset_is_a_new_baseline() {
        let mut a = fec();
        a.on_stats(1000, 10, 0); // primer informe con pérdidas → sube
        let r = a.ratio();
        assert_eq!(a.on_stats(5, 0, 0), None); // reinicio: sin cambios
        assert!((a.ratio() - r).abs() < 1e-6);
        assert!(a.on_stats(65, 1, 0).is_some());
    }
}
