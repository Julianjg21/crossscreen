//! Sincronización bidireccional del portapapeles de texto (F7).
//!
//! - [`text`]: forma canónica del texto (saltos de línea `\n`, sin NUL) y
//!   conversión UTF-8 ⇄ UTF-16 terminada en NUL para las APIs de Windows.
//! - [`ClipboardSync`]: máquina de estados *sans-IO* que decide qué enviar al
//!   par y qué aplicar en local, **sin bucles de eco** entre los dos equipos.
//! - `win` (solo Windows): acceso real al portapapeles Win32 (`CF_UNICODETEXT`).
//!
//! Flujo típico en cada lado:
//! 1. Sondeo periódico: si [`ClipboardSync::should_read`] con el número de
//!    secuencia del SO es `true`, leer el texto local y pasarlo a
//!    [`ClipboardSync::on_local_change`]; si devuelve un mensaje, enviarlo.
//! 2. Al recibir `ControlMessage::ClipboardText`: [`ClipboardSync::on_remote`];
//!    si devuelve texto, escribirlo en el portapapeles local y llamar a
//!    [`ClipboardSync::after_local_write`] con el número de secuencia del SO
//!    posterior a la escritura.

pub mod bridge;
pub mod text;

#[cfg(windows)]
mod win;
pub use bridge::ClipboardBridge;
#[cfg(windows)]
pub use win::Win32Clipboard;

pub use cs_protocol::control::{ClipboardText, MAX_CLIPBOARD_TEXT_BYTES};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClipboardError {
    #[error("texto de portapapeles vacío")]
    Empty,
    #[error("texto de portapapeles demasiado grande: {0} bytes (máx. {MAX_CLIPBOARD_TEXT_BYTES})")]
    TooLarge(usize),
    #[error("el portapapeles está ocupado por otro proceso")]
    Busy,
    #[error("{call} falló con código {code}")]
    Os { call: &'static str, code: i64 },
    #[error("portapapeles no soportado en esta plataforma todavía")]
    Unsupported,
}

/// Huella del texto canónico (para comparar sin guardar copias del contenido).
pub type TextHash = [u8; 32];

pub fn hash_text(canonical: &str) -> TextHash {
    Sha256::digest(canonical.as_bytes()).into()
}

/// Comprueba vacío y límite de tamaño sobre el texto **canónico**.
pub fn validate_text(canonical: &str) -> Result<(), ClipboardError> {
    if canonical.is_empty() {
        return Err(ClipboardError::Empty);
    }
    if canonical.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(ClipboardError::TooLarge(canonical.len()));
    }
    Ok(())
}

/// Qué hacer con un mensaje remoto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    /// Escribir este texto (canónico) en el portapapeles local y luego llamar
    /// a [`ClipboardSync::after_local_write`].
    Apply(String),
    /// El portapapeles local ya tiene ese contenido: nada que hacer.
    AlreadyPresent,
    /// Mensaje duplicado o más viejo que uno ya procesado.
    Stale,
}

/// Estado de sincronización de un lado de la sesión.
///
/// Prevención de eco:
/// - `peer_has` es la huella del último contenido que **sabemos que tiene el
///   par** (lo que le enviamos o lo que nos envió). Nunca se reenvía.
/// - `local_has` es la huella de lo que tiene el portapapeles local según la
///   última lectura o escritura. Un mensaje remoto igual no se reaplica.
/// - `applied_os_seq` es el número de secuencia del SO justo después de que
///   escribimos texto remoto: si el sondeo ve ese mismo número, el cambio es
///   nuestro y ni siquiera hace falta leer.
#[derive(Debug, Default, Clone)]
pub struct ClipboardSync {
    peer_has: Option<TextHash>,
    local_has: Option<TextHash>,
    last_seen_os_seq: Option<u32>,
    applied_os_seq: Option<u32>,
    next_seq: u64,
    last_remote_seq: Option<u64>,
    /// Último texto local rechazado por tamaño, para avisar una sola vez.
    last_rejected: Option<TextHash>,
    stats: SyncStats,
}

/// Contadores para diagnóstico.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncStats {
    pub sent: u64,
    pub applied: u64,
    pub echoes_suppressed: u64,
    pub unchanged_skipped: u64,
    pub rejected_too_large: u64,
    pub stale_remote: u64,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> SyncStats {
        self.stats
    }

    /// Fija el estado inicial al empezar la sesión **sin enviar nada**: lo que
    /// ya estaba copiado no se manda al par (conectar no debe sobrescribir por
    /// sorpresa su portapapeles); solo se sincronizan los cambios posteriores.
    pub fn prime(&mut self, os_seq: u32, text: Option<&str>) {
        self.last_seen_os_seq = Some(os_seq);
        self.local_has = text.map(|t| hash_text(&text::canonicalize(t)));
    }

    /// Olvida el último número de secuencia visto para que el próximo sondeo
    /// vuelva a leer (p. ej. si la lectura falló porque otro proceso tenía el
    /// portapapeles abierto).
    pub fn forget_seen(&mut self) {
        self.last_seen_os_seq = None;
    }

    /// `true` si el portapapeles del SO cambió desde la última vez que se miró
    /// (comparando su número de secuencia) y merece la pena leerlo. Si el
    /// cambio es nuestra propia escritura de texto remoto, devuelve `false` y
    /// lo cuenta como eco suprimido.
    pub fn should_read(&mut self, os_seq: u32) -> bool {
        if self.last_seen_os_seq == Some(os_seq) {
            return false;
        }
        self.last_seen_os_seq = Some(os_seq);
        if self.applied_os_seq == Some(os_seq) {
            self.stats.echoes_suppressed += 1;
            return false;
        }
        true
    }

    /// El portapapeles local cambió (tras `should_read`). `text` es su
    /// contenido de texto tal como lo da el SO (o `None` si no hay texto).
    /// Devuelve el mensaje a enviar al par, si procede.
    pub fn on_local_change(&mut self, text: Option<&str>) -> Option<ClipboardText> {
        let canonical = text::canonicalize(text?);
        let hash = hash_text(&canonical);
        self.local_has = Some(hash);
        if let Err(e) = validate_text(&canonical) {
            if matches!(e, ClipboardError::TooLarge(_)) && self.last_rejected != Some(hash) {
                self.last_rejected = Some(hash);
                self.stats.rejected_too_large += 1;
                tracing::warn!("portapapeles no sincronizado: {e}");
            }
            return None;
        }
        if self.peer_has == Some(hash) {
            // O lo acabamos de recibir del par (eco por contenido) o ya se lo
            // enviamos y no ha cambiado.
            self.stats.unchanged_skipped += 1;
            return None;
        }
        self.peer_has = Some(hash);
        self.next_seq += 1;
        self.stats.sent += 1;
        Some(ClipboardText {
            seq: self.next_seq,
            text: canonical,
        })
    }

    /// Procesa un mensaje del par. No escribe nada: indica qué hacer.
    pub fn on_remote(&mut self, msg: &ClipboardText) -> Result<RemoteAction, ClipboardError> {
        if self.last_remote_seq.is_some_and(|last| msg.seq <= last) {
            self.stats.stale_remote += 1;
            return Ok(RemoteAction::Stale);
        }
        let canonical = text::canonicalize(&msg.text);
        validate_text(&canonical)?;
        self.last_remote_seq = Some(msg.seq);
        let hash = hash_text(&canonical);
        self.peer_has = Some(hash);
        if self.local_has == Some(hash) {
            return Ok(RemoteAction::AlreadyPresent);
        }
        Ok(RemoteAction::Apply(canonical))
    }

    /// Llamar tras escribir en el portapapeles local el texto de
    /// [`RemoteAction::Apply`], con el número de secuencia del SO posterior.
    pub fn after_local_write(&mut self, canonical: &str, os_seq_after: u32) {
        self.local_has = Some(hash_text(canonical));
        self.applied_os_seq = Some(os_seq_after);
        self.stats.applied += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simula un portapapeles de SO: número de secuencia + contenido, con la
    /// conversión de saltos de línea de Windows al escribir.
    #[derive(Default)]
    struct FakeOs {
        seq: u32,
        text: Option<String>,
    }

    impl FakeOs {
        fn user_copies(&mut self, t: &str) {
            self.seq += 1;
            self.text = Some(t.to_owned());
        }
        fn write_from_sync(&mut self, canonical: &str) -> u32 {
            self.seq += 1;
            self.text = Some(text::to_windows_newlines(canonical));
            self.seq
        }
    }

    /// Un lado completo: estado + SO falso. Devuelve lo que enviaría.
    fn poll(sync: &mut ClipboardSync, os: &FakeOs) -> Option<ClipboardText> {
        if sync.should_read(os.seq) {
            sync.on_local_change(os.text.as_deref())
        } else {
            None
        }
    }

    fn deliver(sync: &mut ClipboardSync, os: &mut FakeOs, msg: &ClipboardText) -> RemoteAction {
        let action = sync.on_remote(msg).unwrap();
        if let RemoteAction::Apply(t) = &action {
            let s = os.write_from_sync(t);
            sync.after_local_write(t, s);
        }
        action
    }

    #[test]
    fn no_echo_loop_between_two_peers() {
        let (mut a, mut b) = (ClipboardSync::new(), ClipboardSync::new());
        let (mut os_a, mut os_b) = (FakeOs::default(), FakeOs::default());

        os_a.user_copies("hola\r\nmundo");
        let m = poll(&mut a, &os_a).expect("A envía lo copiado");
        assert_eq!(m.text, "hola\nmundo", "en la red va canónico");
        assert!(matches!(
            deliver(&mut b, &mut os_b, &m),
            RemoteAction::Apply(_)
        ));
        assert_eq!(os_b.text.as_deref(), Some("hola\r\nmundo"));

        // B sondea: ve el cambio que él mismo escribió → no lo devuelve.
        assert!(poll(&mut b, &os_b).is_none());
        assert_eq!(b.stats().echoes_suppressed, 1);
        // A sondea de nuevo sin cambios → nada.
        assert!(poll(&mut a, &os_a).is_none());
        // Varias rondas: el sistema queda en reposo.
        for _ in 0..5 {
            assert!(poll(&mut a, &os_a).is_none());
            assert!(poll(&mut b, &os_b).is_none());
        }
    }

    #[test]
    fn echo_suppressed_by_content_even_if_os_seq_changes() {
        // Otro programa (p. ej. un gestor de portapapeles) reescribe el mismo
        // texto tras nuestra escritura: cambia el número de secuencia, pero el
        // contenido es el que vino del par → no se reenvía.
        let mut b = ClipboardSync::new();
        let mut os = FakeOs::default();
        let m = ClipboardText {
            seq: 1,
            text: "dato".into(),
        };
        deliver(&mut b, &mut os, &m);
        os.user_copies("dato"); // mismo contenido, secuencia nueva
        assert!(poll(&mut b, &os).is_none());
        assert_eq!(b.stats().unchanged_skipped, 1);
    }

    #[test]
    fn recopying_old_text_after_peer_changed_is_sent() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        // El par nos envía A.
        deliver(
            &mut a,
            &mut os,
            &ClipboardText {
                seq: 1,
                text: "A".into(),
            },
        );
        poll(&mut a, &os);
        // Copiamos B: se envía (el par pasa a tener B).
        os.user_copies("B");
        assert_eq!(poll(&mut a, &os).unwrap().text, "B");
        // Volvemos a copiar A: el par tiene B, así que A SÍ debe enviarse.
        os.user_copies("A");
        assert_eq!(poll(&mut a, &os).unwrap().text, "A");
    }

    #[test]
    fn same_local_text_twice_is_not_resent() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        os.user_copies("x");
        assert!(poll(&mut a, &os).is_some());
        os.user_copies("x");
        assert!(poll(&mut a, &os).is_none());
        // Diferencia solo de saltos de línea: es el mismo texto canónico.
        os.user_copies("l1\nl2");
        assert!(poll(&mut a, &os).is_some());
        os.user_copies("l1\r\nl2");
        assert!(poll(&mut a, &os).is_none());
    }

    #[test]
    fn remote_equal_to_local_is_not_rewritten() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        os.user_copies("igual");
        poll(&mut a, &os);
        let before = os.seq;
        let act = deliver(
            &mut a,
            &mut os,
            &ClipboardText {
                seq: 1,
                text: "igual".into(),
            },
        );
        assert_eq!(act, RemoteAction::AlreadyPresent);
        assert_eq!(os.seq, before, "no se reescribe el portapapeles");
    }

    #[test]
    fn stale_and_duplicate_remote_messages_are_ignored() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        let m5 = ClipboardText {
            seq: 5,
            text: "cinco".into(),
        };
        assert!(matches!(
            deliver(&mut a, &mut os, &m5),
            RemoteAction::Apply(_)
        ));
        assert_eq!(deliver(&mut a, &mut os, &m5), RemoteAction::Stale);
        let m3 = ClipboardText {
            seq: 3,
            text: "tres".into(),
        };
        assert_eq!(deliver(&mut a, &mut os, &m3), RemoteAction::Stale);
        assert_eq!(a.stats().stale_remote, 2);
    }

    #[test]
    fn outgoing_seq_increases() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        let mut last = 0;
        for t in ["1", "2", "3"] {
            os.user_copies(t);
            let m = poll(&mut a, &os).unwrap();
            assert!(m.seq > last);
            last = m.seq;
        }
    }

    #[test]
    fn empty_non_text_and_oversized_local_content_is_not_sent() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        os.user_copies("");
        assert!(poll(&mut a, &os).is_none());
        os.seq += 1;
        os.text = None; // p. ej. una imagen: no hay texto
        assert!(poll(&mut a, &os).is_none());
        os.user_copies(&"z".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1));
        assert!(poll(&mut a, &os).is_none());
        assert_eq!(a.stats().rejected_too_large, 1);
        // El mismo texto enorme en otro sondeo no vuelve a contar ni a avisar.
        os.seq += 1;
        assert!(poll(&mut a, &os).is_none());
        assert_eq!(a.stats().rejected_too_large, 1);
        // Justo en el límite sí se envía.
        os.user_copies(&"z".repeat(MAX_CLIPBOARD_TEXT_BYTES));
        assert!(poll(&mut a, &os).is_some());
    }

    #[test]
    fn invalid_remote_messages_are_rejected() {
        let mut a = ClipboardSync::new();
        let empty = ClipboardText {
            seq: 1,
            text: String::new(),
        };
        assert_eq!(a.on_remote(&empty), Err(ClipboardError::Empty));
        let big = ClipboardText {
            seq: 2,
            text: "q".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1),
        };
        assert!(matches!(
            a.on_remote(&big),
            Err(ClipboardError::TooLarge(_))
        ));
        // Solo NUL: canónico vacío → rechazado.
        let nul = ClipboardText {
            seq: 3,
            text: "\0abc".into(),
        };
        assert_eq!(a.on_remote(&nul), Err(ClipboardError::Empty));
    }

    #[test]
    fn prime_does_not_send_existing_content_but_later_changes_are_sent() {
        let mut a = ClipboardSync::new();
        let mut os = FakeOs::default();
        os.user_copies("ya estaba copiado");
        a.prime(os.seq, os.text.as_deref());
        assert!(
            poll(&mut a, &os).is_none(),
            "lo previo a la sesión no se envía"
        );
        // Si el par envía justo ese texto, no hace falta reescribirlo.
        let act = a
            .on_remote(&ClipboardText {
                seq: 1,
                text: "ya estaba copiado".into(),
            })
            .unwrap();
        assert_eq!(act, RemoteAction::AlreadyPresent);
        os.user_copies("nuevo");
        assert_eq!(poll(&mut a, &os).unwrap().text, "nuevo");
    }

    #[test]
    fn forget_seen_forces_reread() {
        let mut a = ClipboardSync::new();
        assert!(a.should_read(5));
        assert!(!a.should_read(5));
        a.forget_seen();
        assert!(a.should_read(5));
    }

    #[test]
    fn unchanged_os_sequence_skips_reading() {
        let mut a = ClipboardSync::new();
        assert!(a.should_read(10));
        assert!(!a.should_read(10));
        assert!(a.should_read(11));
    }
}
