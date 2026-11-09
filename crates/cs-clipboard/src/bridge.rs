//! Puente entre el portapapeles del SO y una sesión: un hilo propio (el
//! portapapeles de Win32 está ligado al hilo que crea la ventana propietaria)
//! que sondea cambios locales y aplica textos remotos.
//!
//! Uso desde un bucle `tokio::select!`:
//! ```ignore
//! let mut clip = ClipboardBridge::spawn(Duration::from_millis(250))?;
//! tokio::select! {
//!     Some(msg) = clip.next_outgoing() => ctl.send(&ControlMessage::ClipboardText(msg)).await?,
//!     m = ctl.recv() => if let Some(ControlMessage::ClipboardText(t)) = m? { clip.deliver(t) },
//! }
//! ```
//!
//! Memoria acotada: los textos remotos pendientes de aplicar no se encolan;
//! hay una sola ranura y **el último sustituye al anterior** (solo importa el
//! contenido más reciente). Así un par no puede agotar la memoria enviando
//! muchos textos de 1 MiB más rápido de lo que se aplican.

use crate::{ClipboardError, ClipboardText, SyncStats};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Intervalo de sondeo recomendado: el portapapeles cambia a escala humana.
pub const DEFAULT_POLL: Duration = Duration::from_millis(250);

/// Ranura "el último gana" para textos remotos, con aviso al hilo.
#[derive(Debug, Default)]
struct Slot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

#[derive(Debug, Default)]
struct SlotState {
    pending: Option<ClipboardText>,
    closed: bool,
    /// Textos remotos sustituidos antes de aplicarse.
    overwritten: u64,
}

#[derive(Debug)]
pub struct ClipboardBridge {
    slot: Arc<Slot>,
    outgoing: tokio::sync::mpsc::UnboundedReceiver<ClipboardText>,
    stats: Arc<Mutex<SyncStats>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardBridge {
    /// Arranca el hilo del portapapeles. El estado inicial se fija sin enviar
    /// nada al par (ver [`crate::ClipboardSync::prime`]).
    pub fn spawn(poll: Duration) -> Result<Self, ClipboardError> {
        #[cfg(windows)]
        let make = crate::Win32Clipboard::new;
        #[cfg(not(windows))]
        let make = crate::LinuxClipboard::new;
        let slot = Arc::new(Slot::default());
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let stats = Arc::new(Mutex::new(SyncStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (sl, st) = (slot.clone(), stats.clone());
        let thread = std::thread::Builder::new()
            .name("cs-clipboard".into())
            .spawn(move || worker(make, poll, sl, out_tx, st, ready_tx))
            .map_err(|_| ClipboardError::Os {
                call: "thread::spawn",
                code: 0,
            })?;
        // Esperar a que el backend exista para informar errores.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(ClipboardError::Unsupported),
        }
        Ok(ClipboardBridge {
            slot,
            outgoing: out_rx,
            stats,
            thread: Some(thread),
        })
    }

    /// Texto recibido del par: se aplicará al portapapeles local (sin eco).
    /// Si aún había otro pendiente, este lo sustituye.
    pub fn deliver(&self, msg: ClipboardText) {
        if let Ok(mut s) = self.slot.state.lock() {
            if s.pending.replace(msg).is_some() {
                s.overwritten += 1;
            }
        }
        self.slot.cv.notify_one();
    }

    /// Siguiente texto local que hay que enviar al par. `None` si el hilo terminó.
    pub async fn next_outgoing(&mut self) -> Option<ClipboardText> {
        self.outgoing.recv().await
    }

    pub fn stats(&self) -> SyncStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Textos remotos que se sustituyeron antes de poder aplicarse.
    pub fn remote_overwritten(&self) -> u64 {
        self.slot.state.lock().map(|s| s.overwritten).unwrap_or(0)
    }
}

impl Drop for ClipboardBridge {
    fn drop(&mut self) {
        if let Ok(mut s) = self.slot.state.lock() {
            s.closed = true;
        }
        self.slot.cv.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `Option<ClipboardBridge>` usable en `select!`: si no hay puente, nunca resuelve.
pub async fn next_outgoing(bridge: &mut Option<ClipboardBridge>) -> Option<ClipboardText> {
    match bridge {
        Some(b) => b.next_outgoing().await,
        None => std::future::pending().await,
    }
}

/// Espera hasta `poll` a que llegue un texto remoto. `Err(())` si se cerró.
fn wait_remote(slot: &Slot, poll: Duration) -> Result<Option<ClipboardText>, ()> {
    let guard = slot.state.lock().map_err(|_| ())?;
    let (mut guard, _) = slot
        .cv
        .wait_timeout_while(guard, poll, |s| s.pending.is_none() && !s.closed)
        .map_err(|_| ())?;
    if guard.closed {
        return Err(());
    }
    Ok(guard.pending.take())
}

/// Operaciones del portapapeles del SO que usa el hilo del puente. Los
/// backends viven en su propio hilo (Win32 exige el hilo de la ventana
/// propietaria; X11, un único dueño de la conexión).
trait Backend {
    /// Contador que avanza cuando cambia el contenido.
    fn sequence_number(&self) -> u32;
    /// Texto actual (sin normalizar; se canonicaliza al comparar).
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;
    /// Escribe texto canónico; devuelve el contador tras escribir.
    fn write_text(&self, canonical: &str) -> Result<u32, ClipboardError>;
}

#[cfg(windows)]
impl Backend for crate::Win32Clipboard {
    fn sequence_number(&self) -> u32 {
        crate::Win32Clipboard::sequence_number(self)
    }
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        crate::Win32Clipboard::read_text(self)
    }
    fn write_text(&self, canonical: &str) -> Result<u32, ClipboardError> {
        crate::Win32Clipboard::write_text(self, canonical)
    }
}

#[cfg(not(windows))]
impl Backend for crate::LinuxClipboard {
    fn sequence_number(&self) -> u32 {
        crate::LinuxClipboard::sequence_number(self)
    }
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        crate::LinuxClipboard::read_text(self)
    }
    fn write_text(&self, canonical: &str) -> Result<u32, ClipboardError> {
        crate::LinuxClipboard::write_text(self, canonical)
    }
}

fn worker<B: Backend>(
    make: fn() -> Result<B, ClipboardError>,
    poll: Duration,
    slot: Arc<Slot>,
    outgoing: tokio::sync::mpsc::UnboundedSender<ClipboardText>,
    stats: Arc<Mutex<SyncStats>>,
    ready: std::sync::mpsc::Sender<Result<(), ClipboardError>>,
) {
    use crate::{ClipboardSync, RemoteAction};

    let cb = match make() {
        Ok(cb) => cb,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut sync = ClipboardSync::new();
    let seq = cb.sequence_number();
    match cb.read_text() {
        Ok(t) => sync.prime(seq, t.as_deref()),
        Err(e) => {
            // No "fail-open": lo copiado antes de conectar no se enviará aunque
            // la primera lectura válida llegue más tarde.
            tracing::debug!("portapapeles: lectura inicial fallida ({e}); se reintentará");
            sync.prime_unread(seq);
        }
    }
    let _ = ready.send(Ok(()));
    // Texto remoto pendiente de aplicar (si el portapapeles estaba ocupado).
    let mut pending: Option<String> = None;

    loop {
        // Esperar un texto remoto o el siguiente sondeo, lo que llegue antes.
        match wait_remote(&slot, poll) {
            Ok(Some(msg)) => match sync.on_remote(&msg) {
                Ok(RemoteAction::Apply(t)) => pending = Some(t),
                Ok(RemoteAction::AlreadyPresent | RemoteAction::Stale) => {}
                Err(e) => tracing::warn!("portapapeles remoto rechazado: {e}"),
            },
            Ok(None) => {}
            Err(()) => break,
        }
        if let Some(t) = pending.take() {
            match cb.write_text(&t) {
                Ok(seq_after) => sync.after_local_write(&t, seq_after),
                Err(ClipboardError::Busy) => pending = Some(t), // reintento en el próximo ciclo
                Err(e) => tracing::warn!("no se pudo escribir el portapapeles: {e}"),
            }
        }
        if sync.should_read(cb.sequence_number()) {
            match cb.read_text() {
                Ok(t) => {
                    if let Some(msg) = sync.on_local_change(t.as_deref()) {
                        if outgoing.send(msg).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("portapapeles ocupado al leer ({e}); se reintenta");
                    sync.forget_seen();
                }
            }
        }
        if let Ok(mut s) = stats.lock() {
            *s = sync.stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(seq: u64, n: usize) -> ClipboardText {
        ClipboardText {
            seq,
            text: "x".repeat(n),
        }
    }

    #[test]
    fn slot_keeps_only_latest_remote_text() {
        let slot = Slot::default();
        for i in 1..=1000 {
            let mut s = slot.state.lock().unwrap();
            if s.pending.replace(msg(i, 1024)).is_some() {
                s.overwritten += 1;
            }
        }
        let got = wait_remote(&slot, Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(got.seq, 1000, "solo queda el último");
        assert_eq!(slot.state.lock().unwrap().overwritten, 999);
        assert!(wait_remote(&slot, Duration::from_millis(1))
            .unwrap()
            .is_none());
    }

    #[test]
    fn wait_wakes_on_delivery_and_reports_close() {
        let slot = Arc::new(Slot::default());
        let s2 = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.state.lock().unwrap().pending = Some(msg(7, 3));
            s2.cv.notify_one();
        });
        let start = std::time::Instant::now();
        let got = wait_remote(&slot, Duration::from_secs(5)).unwrap();
        assert_eq!(got.unwrap().seq, 7);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "despierta sin esperar al sondeo"
        );
        t.join().unwrap();
        slot.state.lock().unwrap().closed = true;
        assert!(wait_remote(&slot, Duration::from_secs(5)).is_err());
    }
}
