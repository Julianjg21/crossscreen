//! Limitador de intentos fallidos (emparejamiento).
//!
//! Política: como máximo `max_failures` fallos dentro de `window`; al
//! superarlo, bloqueo de `base_lockout × 2^(n-1)` (n = número de bloqueos
//! consecutivos), con techo `max_lockout`. Un éxito reinicia el contador.
//! El tiempo se pasa explícitamente para que sea determinista en tests.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct AttemptLimiter {
    max_failures: usize,
    window: Duration,
    base_lockout: Duration,
    max_lockout: Duration,
    failures: VecDeque<Instant>,
    locked_until: Option<Instant>,
    consecutive_lockouts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("demasiados intentos; reintenta en {retry_after:?}")]
pub struct Locked {
    pub retry_after: Duration,
}

impl Default for AttemptLimiter {
    fn default() -> Self {
        Self::new(
            5,
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        )
    }
}

impl AttemptLimiter {
    pub fn new(
        max_failures: usize,
        window: Duration,
        base_lockout: Duration,
        max_lockout: Duration,
    ) -> Self {
        AttemptLimiter {
            max_failures,
            window,
            base_lockout,
            max_lockout,
            failures: VecDeque::new(),
            locked_until: None,
            consecutive_lockouts: 0,
        }
    }

    /// Comprueba si se permite un intento ahora.
    pub fn check(&self, now: Instant) -> Result<(), Locked> {
        match self.locked_until {
            Some(until) if now < until => Err(Locked {
                retry_after: until - now,
            }),
            _ => Ok(()),
        }
    }

    pub fn record_failure(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= self.max_failures {
            self.consecutive_lockouts = self.consecutive_lockouts.saturating_add(1);
            let factor = 1u32 << (self.consecutive_lockouts - 1).min(16);
            let lockout = self
                .base_lockout
                .saturating_mul(factor)
                .min(self.max_lockout);
            self.locked_until = Some(now + lockout);
            self.failures.clear();
        }
    }

    pub fn record_success(&mut self) {
        self.failures.clear();
        self.locked_until = None;
        self.consecutive_lockouts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> AttemptLimiter {
        AttemptLimiter::new(
            3,
            Duration::from_secs(60),
            Duration::from_secs(10),
            Duration::from_secs(25),
        )
    }

    #[test]
    fn locks_after_max_failures_with_exponential_backoff() {
        let t0 = Instant::now();
        let mut l = limiter();
        for _ in 0..2 {
            l.record_failure(t0);
            assert!(l.check(t0).is_ok());
        }
        l.record_failure(t0);
        assert_eq!(
            l.check(t0),
            Err(Locked {
                retry_after: Duration::from_secs(10)
            })
        );
        assert!(l.check(t0 + Duration::from_secs(10)).is_ok());

        let t1 = t0 + Duration::from_secs(11);
        for _ in 0..3 {
            l.record_failure(t1);
        }
        assert_eq!(
            l.check(t1),
            Err(Locked {
                retry_after: Duration::from_secs(20)
            })
        );

        let t2 = t1 + Duration::from_secs(21);
        for _ in 0..3 {
            l.record_failure(t2);
        }
        // 40 s recortado al techo de 25 s.
        assert_eq!(
            l.check(t2),
            Err(Locked {
                retry_after: Duration::from_secs(25)
            })
        );
    }

    #[test]
    fn failures_outside_window_expire() {
        let t0 = Instant::now();
        let mut l = limiter();
        l.record_failure(t0);
        l.record_failure(t0);
        l.record_failure(t0 + Duration::from_secs(61));
        assert!(l.check(t0 + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn success_resets() {
        let t0 = Instant::now();
        let mut l = limiter();
        l.record_failure(t0);
        l.record_failure(t0);
        l.record_success();
        l.record_failure(t0);
        assert!(l.check(t0).is_ok());
    }
}
