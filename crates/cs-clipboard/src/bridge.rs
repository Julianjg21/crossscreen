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

use crate::{ClipboardError, ClipboardText, SyncStats};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Intervalo de sondeo recomendado: el portapapeles cambia a escala humana.
pub const DEFAULT_POLL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct ClipboardBridge {
    incoming: std::sync::mpsc::Sender<ClipboardText>,
    outgoing: tokio::sync::mpsc::UnboundedReceiver<ClipboardText>,
    stats: Arc<Mutex<SyncStats>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardBridge {
    /// Arranca el hilo del portapapeles. El estado inicial se fija sin enviar
    /// nada al par (ver [`crate::ClipboardSync::prime`]).
    pub fn spawn(poll: Duration) -> Result<Self, ClipboardError> {
        #[cfg(windows)]
        {
            let (in_tx, in_rx) = std::sync::mpsc::channel::<ClipboardText>();
            let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
            let stats = Arc::new(Mutex::new(SyncStats::default()));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let st = stats.clone();
            let thread = std::thread::Builder::new()
                .name("cs-clipboard".into())
                .spawn(move || win_worker(poll, in_rx, out_tx, st, ready_tx))
                .map_err(|_| ClipboardError::Os {
                    call: "thread::spawn",
                    code: 0,
                })?;
            // Esperar a que la ventana propietaria exista para informar errores.
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = thread.join();
                    return Err(e);
                }
                Err(_) => return Err(ClipboardError::Unsupported),
            }
            Ok(ClipboardBridge {
                incoming: in_tx,
                outgoing: out_rx,
                stats,
                thread: Some(thread),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = poll;
            Err(ClipboardError::Unsupported)
        }
    }

    /// Texto recibido del par: se aplicará al portapapeles local (sin eco).
    pub fn deliver(&self, msg: ClipboardText) {
        let _ = self.incoming.send(msg);
    }

    /// Siguiente texto local que hay que enviar al par. `None` si el hilo terminó.
    pub async fn next_outgoing(&mut self) -> Option<ClipboardText> {
        self.outgoing.recv().await
    }

    pub fn stats(&self) -> SyncStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }
}

impl Drop for ClipboardBridge {
    fn drop(&mut self) {
        // Cerrar el canal de entrada despierta y termina el hilo.
        let (dead_tx, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.incoming, dead_tx));
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

#[cfg(windows)]
fn win_worker(
    poll: Duration,
    incoming: std::sync::mpsc::Receiver<ClipboardText>,
    outgoing: tokio::sync::mpsc::UnboundedSender<ClipboardText>,
    stats: Arc<Mutex<SyncStats>>,
    ready: std::sync::mpsc::Sender<Result<(), ClipboardError>>,
) {
    use crate::{ClipboardSync, RemoteAction, Win32Clipboard};
    use std::sync::mpsc::RecvTimeoutError;

    let cb = match Win32Clipboard::new() {
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
            tracing::debug!("portapapeles: lectura inicial fallida ({e}); se reintentará");
            sync.prime(seq, None);
            sync.forget_seen();
        }
    }
    let _ = ready.send(Ok(()));
    // Texto remoto pendiente de aplicar (si el portapapeles estaba ocupado).
    let mut pending: Option<String> = None;

    loop {
        // Esperar un texto remoto o el siguiente sondeo, lo que llegue antes.
        match incoming.recv_timeout(poll) {
            Ok(msg) => match sync.on_remote(&msg) {
                Ok(RemoteAction::Apply(t)) => pending = Some(t),
                Ok(RemoteAction::AlreadyPresent | RemoteAction::Stale) => {}
                Err(e) => tracing::warn!("portapapeles remoto rechazado: {e}"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
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
