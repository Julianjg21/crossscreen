//! Reloj monótono en microsegundos.
//!
//! - **Windows:** QPC, el mismo que usa la fuente para `pts_us` (contrato
//!   §Vídeo: instante de captura en µs de `QueryPerformanceCounter`, módulo
//!   2³²). Con fuente y visor en el mismo equipo permite medir la latencia
//!   captura → presentación.
//! - **Linux:** reloj monótono del proceso. Solo sirve para medidas
//!   relativas (*jitter*); la latencia "mismo equipo" no tiene sentido entre
//!   un Windows y un Linux.

use std::time::Duration;

#[cfg(windows)]
pub use qpc::qpc_us;

#[cfg(not(windows))]
pub fn qpc_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(windows)]
mod qpc {
    #![allow(unsafe_code)] // FFI QueryPerformanceCounter/Frequency.

    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    fn freq() -> i64 {
        static FREQ: OnceLock<i64> = OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: escribe en un i64 local; no puede fallar desde Windows XP.
            let _ = unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }

    /// QPC actual en microsegundos (64 bits).
    pub fn qpc_us() -> u64 {
        let mut c = 0i64;
        // SAFETY: escribe en un i64 local.
        let _ = unsafe { QueryPerformanceCounter(&mut c) };
        (c.max(0) as u128 * 1_000_000 / freq() as u128) as u64
    }
}

/// QPC actual en µs módulo 2³² (formato de `pts_us`).
pub fn qpc_us32() -> u32 {
    qpc_us() as u32
}

/// Latencia desde `pts_us` (µs QPC mod 2³²) hasta `now_us` con el mismo
/// formato, tolerando la vuelta de 2³² µs (~71,6 min). Devuelve `None` si la
/// diferencia es «negativa» (≥ 2³¹ µs tras el *wrapping*), lo que indica
/// relojes no comparables o un pts corrupto.
pub fn latency_since(pts_us: u32, now_us: u32) -> Option<Duration> {
    let d = now_us.wrapping_sub(pts_us);
    (d < 1 << 31).then(|| Duration::from_micros(u64::from(d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_wraps() {
        assert_eq!(latency_since(100, 350), Some(Duration::from_micros(250)));
        // Vuelta de 2^32: pts justo antes, ahora justo después.
        assert_eq!(
            latency_since(u32::MAX - 99, 400),
            Some(Duration::from_micros(500))
        );
        // pts en el «futuro»: no comparable.
        assert_eq!(latency_since(1_000, 500), None);
    }

    #[test]
    fn qpc_monotonic() {
        let a = qpc_us();
        let b = qpc_us();
        assert!(b >= a);
    }
}
