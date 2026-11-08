//! Prioridad del hilo de captura + codificación (hallazgo de la PoC-2: con
//! la CPU cargada, sin prioridad alta las colas de la captura llegan a
//! decenas de ms).
#![allow(unsafe_code)] // FFI avrt/kernel32; cada bloque unsafe está acotado y comentado.

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
    GetCurrentThread, SetThreadPriority, AVRT_PRIORITY_HIGH, THREAD_PRIORITY_HIGHEST,
};

/// Prioridad obtenida para el hilo actual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// MMCSS, tarea "Games" con `AVRT_PRIORITY_HIGH`.
    MmcssGames,
    /// `THREAD_PRIORITY_HIGHEST` (MMCSS no disponible).
    Highest,
    /// No se pudo subir la prioridad.
    Normal,
}

/// Sube la prioridad del hilo actual mientras viva; la restaura en `Drop`.
/// Debe crearse y destruirse en el mismo hilo (no es `Send`).
#[derive(Debug)]
pub struct ThreadPriorityGuard {
    mmcss: Option<HANDLE>,
    pub priority: Priority,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ThreadPriorityGuard {
    pub fn raise() -> Self {
        let mut index = 0u32;
        // SAFETY: nombre de tarea estático terminado en NUL; `index` es un
        // u32 local válido durante la llamada.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Games"), &mut index) } {
            Ok(h) if !h.is_invalid() => {
                // SAFETY: `h` es el handle MMCSS recién obtenido.
                let _ = unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_HIGH) };
                Self {
                    mmcss: Some(h),
                    priority: Priority::MmcssGames,
                    _not_send: Default::default(),
                }
            }
            other => {
                tracing::warn!(
                    ?other,
                    "MMCSS no disponible; se usa THREAD_PRIORITY_HIGHEST"
                );
                // SAFETY: pseudo-handle del hilo actual; no hay que cerrarlo.
                let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
                    .is_ok();
                Self {
                    mmcss: None,
                    priority: if ok {
                        Priority::Highest
                    } else {
                        Priority::Normal
                    },
                    _not_send: Default::default(),
                }
            }
        }
    }
}

impl Drop for ThreadPriorityGuard {
    fn drop(&mut self) {
        if let Some(h) = self.mmcss.take() {
            // SAFETY: handle devuelto por AvSetMmThreadCharacteristicsW en este hilo.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}
