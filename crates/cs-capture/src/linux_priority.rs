//! Prioridad de los hilos de captura y codificación en Linux.
//!
//! Con el equipo cargado (compilaciones, navegador, poca memoria libre) el
//! planificador reparte la CPU a partes iguales y el emisor pierde frames:
//! la pantalla extendida se retrasa y llega a congelarse. Un usuario normal
//! no puede bajar el *nice* de sus hilos, pero RealtimeKit (`rtkit`, el
//! servicio que ya usa PipeWire para su audio) lo concede por D-Bus a las
//! sesiones activas. Si no está disponible, se sigue con prioridad normal.
//!
//! Medido en Ubuntu (GNOME): rtkit concede *nice* −11 al hilo que lo pide.

use std::time::Duration;

/// *Nice* que se pide: el mismo que usa PipeWire para sus hilos.
pub const HIGH_PRIORITY_NICE: i32 = -11;

const RTKIT_DEST: &str = "org.freedesktop.RealtimeKit1";
const RTKIT_PATH: &str = "/org/freedesktop/RealtimeKit1";
/// Una llamada a rtkit tarda milisegundos; no bloquear el hilo más que esto.
const RTKIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Sube la prioridad del hilo que llama (*nice* [`HIGH_PRIORITY_NICE`]).
///
/// rtkit marca el hilo con `SCHED_RESET_ON_FORK`: los hilos que cree después
/// **no** la heredan (medido), así que cada hilo que la necesite debe
/// pedirla. Bloquea unos milisegundos: llamar al principio del hilo, nunca
/// desde código async.
pub fn raise_current_thread() -> Result<(), String> {
    request(rustix::thread::gettid().as_raw_nonzero().get())
}

/// Como [`raise_current_thread`], dejando constancia en el registro.
pub fn raise_current_thread_logged(what: &str) {
    match raise_current_thread() {
        Ok(()) => tracing::info!(
            "prioridad alta para el hilo de {what} (nice {HIGH_PRIORITY_NICE}, rtkit)"
        ),
        Err(e) => tracing::info!(
            "el hilo de {what} sigue con prioridad normal (rtkit no disponible: {e})"
        ),
    }
}

fn request(tid: i32) -> Result<(), String> {
    let thread = u64::try_from(tid).map_err(|_| format!("tid inválido {tid}"))?;
    // Runtime mínimo y propio: este hilo no es de tokio y zbus lo necesita.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        tokio::time::timeout(RTKIT_TIMEOUT, async {
            let conn = zbus::Connection::system()
                .await
                .map_err(|e| e.to_string())?;
            conn.call_method(
                Some(RTKIT_DEST),
                RTKIT_PATH,
                Some(RTKIT_DEST),
                "MakeThreadHighPriority",
                &(thread, HIGH_PRIORITY_NICE),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "sin respuesta".to_owned())?
    })
}
