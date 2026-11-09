//! Arranque de captura de pantalla mediante XDG Desktop Portal.
//!
//! El portal entrega un nodo PipeWire y un descriptor de conexión después de
//! la autorización interactiva del usuario. La sesión del portal se conserva
//! en [`PortalCaptureSession`] y debe cerrarse con
//! [`PortalSession::close`] al terminar: ashpd no la cierra al
//! soltarla (su conexión D-Bus es global y vive todo el proceso), así que sin
//! cerrarla GNOME mantiene el indicador de "compartiendo pantalla".

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SourceType, Stream},
    PersistMode, Session,
};
use std::os::fd::OwnedFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalStreamInfo {
    pub node_id: u32,
    pub size: Option<(i32, i32)>,
}

pub struct PortalCaptureSession {
    pub stream: PortalStreamInfo,
    pub pipewire_fd: OwnedFd,
    /// Sesión del portal: hay que cerrarla con [`PortalSession::close`].
    pub session: PortalSession,
}

/// Sesión abierta del portal de captura.
pub struct PortalSession(Session<'static, Screencast<'static>>);

impl std::fmt::Debug for PortalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PortalSession")
    }
}

impl PortalSession {
    /// Se resuelve cuando el sistema cierra la sesión (p. ej. el usuario pulsa
    /// «Dejar de compartir» en GNOME). Si no se puede escuchar la señal, no se
    /// resuelve nunca.
    pub async fn wait_closed(&self) {
        use futures_util::StreamExt;
        match self.0.receive_closed().await {
            Ok(closed) => {
                let mut closed = std::pin::pin!(closed);
                let _ = closed.next().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }

    /// Cierra la sesión del portal: GNOME deja de compartir y apaga su
    /// indicador.
    pub async fn close(self) {
        if let Err(e) = self.0.close().await {
            tracing::debug!("cerrando la sesión del portal: {e}");
        }
    }
}

impl std::fmt::Debug for PortalCaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalCaptureSession")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// Modo de cursor preferido entre los que ofrece el portal: incrustado en el
/// vídeo (el receptor no recibe la forma del cursor por separado desde
/// Linux); si no, oculto.
fn pick_cursor_mode(available: ashpd::enumflags2::BitFlags<CursorMode>) -> CursorMode {
    if available.contains(CursorMode::Embedded) {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    }
}

impl PortalCaptureSession {
    /// Solicita al usuario un monitor completo y abre el PipeWire remoto.
    ///
    /// Muestra el diálogo del sistema; si el usuario lo cancela devuelve
    /// `ashpd::Error::Response(ResponseError::Cancelled)`.
    pub async fn open() -> Result<Self, ashpd::Error> {
        let proxy: Screencast<'static> = Screencast::new().await?;
        let session = proxy.create_session().await?;
        let result = async {
            let cursor = match proxy.available_cursor_modes().await {
                Ok(modes) => pick_cursor_mode(modes),
                Err(_) => CursorMode::Embedded,
            };
            proxy
                .select_sources(
                    &session,
                    cursor,
                    SourceType::Monitor.into(),
                    false,
                    None,
                    PersistMode::DoNot,
                )
                .await?
                .response()?;
            let response = proxy.start(&session, None).await?.response()?;
            let stream = response
                .streams()
                .first()
                .map(stream_info)
                .ok_or(ashpd::Error::NoResponse)?;
            let pipewire_fd = proxy.open_pipe_wire_remote(&session).await?;
            Ok::<_, ashpd::Error>((stream, pipewire_fd))
        }
        .await;
        match result {
            Ok((stream, pipewire_fd)) => Ok(Self {
                stream,
                pipewire_fd,
                session: PortalSession(session),
            }),
            Err(e) => {
                // No dejar la sesión abierta (ni el indicador de GNOME).
                let _ = session.close().await;
                Err(e)
            }
        }
    }
}

/// Mensaje claro para el usuario a partir de un error del portal.
pub fn describe_error(e: &ashpd::Error) -> String {
    use ashpd::desktop::ResponseError;
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => {
            "se canceló el diálogo para compartir la pantalla".into()
        }
        ashpd::Error::Response(ResponseError::Other) => {
            "el sistema rechazó compartir la pantalla".into()
        }
        ashpd::Error::PortalNotFound(_) => {
            "no hay XDG Desktop Portal con captura de pantalla (instala xdg-desktop-portal-gnome o el de tu escritorio)".into()
        }
        other => format!("XDG Desktop Portal: {other}"),
    }
}

fn stream_info(stream: &Stream) -> PortalStreamInfo {
    PortalStreamInfo {
        node_id: stream.pipe_wire_node_id(),
        size: stream.size(),
    }
}

#[cfg(test)]
mod tests {
    use super::{pick_cursor_mode, PortalStreamInfo};
    use ashpd::desktop::screencast::CursorMode;

    #[test]
    fn stream_info_is_copyable() {
        let info = PortalStreamInfo {
            node_id: 42,
            size: Some((1920, 1080)),
        };
        assert_eq!(info, info);
    }

    #[test]
    fn cursor_mode_prefers_embedded() {
        assert_eq!(
            pick_cursor_mode(CursorMode::Embedded | CursorMode::Hidden),
            CursorMode::Embedded
        );
        assert_eq!(
            pick_cursor_mode(CursorMode::Hidden.into()),
            CursorMode::Hidden
        );
        assert_eq!(pick_cursor_mode(Default::default()), CursorMode::Hidden);
    }
}
