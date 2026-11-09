//! Arranque de captura de pantalla mediante XDG Desktop Portal.
//!
//! El portal entrega un nodo PipeWire y un descriptor de conexión después de
//! la autorización interactiva del usuario. El consumo de buffers se mantiene
//! separado para que la sesión no dependa de una implementación concreta del
//! bucle de PipeWire.

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SourceType, Stream},
    PersistMode,
};
use std::os::fd::OwnedFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalStreamInfo {
    pub node_id: u32,
    pub size: Option<(i32, i32)>,
}

#[derive(Debug)]
pub struct PortalCaptureSession {
    pub stream: PortalStreamInfo,
    pub pipewire_fd: OwnedFd,
}

impl PortalCaptureSession {
    /// Solicita al usuario un monitor completo y abre el PipeWire remoto.
    ///
    /// El `restore_token` se reserva para una futura reconexión persistente;
    /// esta primera sesión solicita autorización explícita cada vez.
    pub async fn open() -> Result<Self, ashpd::Error> {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session().await?;
        proxy
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
        let response = proxy.start(&session, None).await?.response()?;
        let stream = response.streams().first().ok_or(ashpd::Error::NoResponse)?;
        let pipewire_fd = proxy.open_pipe_wire_remote(&session).await?;
        Ok(Self {
            stream: stream_info(stream),
            pipewire_fd,
        })
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
    use super::PortalStreamInfo;

    #[test]
    fn stream_info_is_copyable() {
        let info = PortalStreamInfo {
            node_id: 42,
            size: Some((1920, 1080)),
        };
        assert_eq!(info, info);
    }
}
