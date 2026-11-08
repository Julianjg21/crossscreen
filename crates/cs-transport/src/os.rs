//! Utilidades dependientes del SO para el *pacing* y las métricas del banco:
//! espera de alta resolución y tiempo de CPU del proceso.

#![allow(unsafe_code)]

use std::time::Duration;

/// Tiempo de CPU (usuario + núcleo) consumido por el proceso actual desde que
/// arrancó. `None` si no está implementado en esta plataforma o falla.
#[cfg(windows)]
pub fn process_cpu_time() -> Option<Duration> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: `GetCurrentProcess` devuelve un pseudo-handle siempre válido que
    // no hay que cerrar; los cuatro punteros apuntan a `FILETIME` locales.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // FILETIME cuenta intervalos de 100 ns.
    Some(Duration::from_nanos((ticks(k) + ticks(u)) * 100))
}

/// Ver la versión de Windows. En otras plataformas aún no se mide.
#[cfg(not(windows))]
pub fn process_cpu_time() -> Option<Duration> {
    None
}

/// Duerme el hilo actual `d` con la mejor resolución disponible.
///
/// En Windows usa un temporizador *waitable* creado con
/// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803+), uno por hilo y
/// reutilizado; si no se puede crear, cae a `std::thread::sleep`. En el resto
/// de plataformas `std::thread::sleep` (nanosleep) ya tiene resolución fina.
pub fn high_res_sleep(d: Duration) {
    #[cfg(windows)]
    {
        if win::sleep(d) {
            return;
        }
    }
    std::thread::sleep(d);
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::time::Duration;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
    };

    struct Timer(HANDLE);

    impl Drop for Timer {
        fn drop(&mut self) {
            // SAFETY: el handle lo creó `CreateWaitableTimerExW` y solo se
            // cierra aquí, una vez.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    thread_local! {
        static TIMER: RefCell<Option<Option<Timer>>> = const { RefCell::new(None) };
    }

    fn create() -> Option<Timer> {
        // SAFETY: sin atributos de seguridad ni nombre (punteros nulos
        // permitidos por la API); el handle devuelto se envuelve en `Timer`.
        unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok()
        .map(Timer)
    }

    /// Devuelve `false` si no hay temporizador de alta resolución.
    pub(super) fn sleep(d: Duration) -> bool {
        TIMER.with(|cell| {
            let mut slot = cell.borrow_mut();
            let Some(timer) = slot.get_or_insert_with(create) else {
                return false;
            };
            // Tiempo relativo: negativo, en unidades de 100 ns.
            let due: i64 = -((d.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
            // SAFETY: `timer.0` es un handle de temporizador válido de este
            // hilo; `due` vive durante la llamada; sin rutina APC.
            let ok = unsafe { SetWaitableTimer(timer.0, &due, 0, None, None, false) }.is_ok();
            if !ok {
                return false;
            }
            // SAFETY: espera sobre el handle válido recién armado.
            unsafe { WaitForSingleObject(timer.0, INFINITE) == WAIT_OBJECT_0 }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn high_res_sleep_waits_at_least_the_duration() {
        let t = Instant::now();
        high_res_sleep(Duration::from_micros(500));
        assert!(t.elapsed() >= Duration::from_micros(500));
    }

    #[cfg(windows)]
    #[test]
    fn cpu_time_is_monotonic() {
        let a = process_cpu_time().unwrap();
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        assert!(process_cpu_time().unwrap() >= a);
    }
}
