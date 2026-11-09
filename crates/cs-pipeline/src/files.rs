//! Transferencia de archivos en el emisor (ADR-0010).
//!
//! Solo si `SenderConfig::files` está puesto (desactivado por defecto): se
//! anuncia `FilesAvailable` al receptor y se arranca el servicio de
//! `cs-files` sobre la conexión. Tras una reconexión se vuelve a arrancar
//! sobre la conexión nueva (la cola de salida se conserva).

use crate::SenderConfig;
use cs_protocol::control::ControlMessage;
use cs_transport::{Connection, FramedStream};

/// Arranca el servicio (si está activado) y lo anuncia al receptor.
pub(crate) async fn start(
    conn: &Connection,
    cfg: &SenderConfig,
    ctl: &mut FramedStream,
) -> Option<cs_files::FileService> {
    let handle = cfg.files.as_ref()?;
    if let Err(e) = ctl
        .send(&ControlMessage::FilesAvailable { enabled: true })
        .await
    {
        tracing::warn!("no se pudo anunciar la transferencia de archivos: {e}");
        return None;
    }
    tracing::info!(inbox = %handle.inbox().display(), "transferencia de archivos activada");
    Some(cs_files::FileService::start(conn.clone(), handle))
}

/// El receptor anunció si acepta archivos.
pub(crate) fn on_peer(files: &Option<cs_files::FileService>, enabled: bool) {
    if let Some(f) = files {
        f.set_peer_enabled(enabled);
    }
}
