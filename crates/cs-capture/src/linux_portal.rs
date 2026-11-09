//! Arranque de captura de pantalla mediante XDG Desktop Portal.
//!
//! El portal entrega un nodo PipeWire y un descriptor de conexión después de
//! la autorización interactiva del usuario. Si el sistema ofrece el portal
//! `RemoteDesktop`, se pide en **el mismo diálogo** compartir la pantalla y
//! permitir el control remoto (ratón y teclado); si no, solo la pantalla.
//!
//! La sesión se conserva en [`PortalSession`] y debe cerrarse con
//! [`PortalSession::close`] al terminar: ashpd no la cierra al soltarla (su
//! conexión D-Bus es global y vive todo el proceso), así que sin cerrarla
//! GNOME mantiene el indicador de "compartiendo pantalla".

use crate::linux_input::InputAction;
use ashpd::desktop::{
    remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop},
    screencast::{CursorMode, Screencast, SourceType, Stream},
    PersistMode, ResponseError, Session,
};
use ashpd::enumflags2::BitFlags;
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

/// Sesión abierta del portal (solo pantalla, o pantalla + control remoto).
pub struct PortalSession(Inner);

enum Inner {
    Screen(Session<'static, Screencast<'static>>),
    Remote {
        rd: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
        node_id: u32,
        devices: BitFlags<DeviceType>,
    },
}

impl std::fmt::Debug for PortalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalSession")
            .field("input", &self.can_inject())
            .finish()
    }
}

impl PortalSession {
    /// ¿El usuario permitió ratón o teclado?
    pub fn can_inject(&self) -> bool {
        match &self.0 {
            Inner::Screen(_) => false,
            Inner::Remote { devices, .. } => {
                devices.intersects(DeviceType::Keyboard | DeviceType::Pointer)
            }
        }
    }

    /// Inyecta una acción de entrada. Sin control remoto concedido no hace nada.
    pub async fn inject(&self, action: &InputAction) -> Result<(), ashpd::Error> {
        let Inner::Remote {
            rd,
            session,
            node_id,
            devices,
        } = &self.0
        else {
            return Ok(());
        };
        let state = |pressed: bool| {
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            }
        };
        match *action {
            InputAction::PointerAbs { x, y } if devices.contains(DeviceType::Pointer) => {
                rd.notify_pointer_motion_absolute(session, *node_id, x, y)
                    .await
            }
            InputAction::Button { code, pressed } if devices.contains(DeviceType::Pointer) => {
                rd.notify_pointer_button(session, code as i32, state(pressed))
                    .await
            }
            InputAction::Axis { horizontal, steps } if devices.contains(DeviceType::Pointer) => {
                let axis = if horizontal {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                rd.notify_pointer_axis_discrete(session, axis, steps).await
            }
            InputAction::Key { code, pressed } if devices.contains(DeviceType::Keyboard) => {
                rd.notify_keyboard_keycode(session, code as i32, state(pressed))
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Se resuelve cuando el sistema cierra la sesión (p. ej. el usuario pulsa
    /// «Dejar de compartir» en GNOME). Si no se puede escuchar la señal, no se
    /// resuelve nunca.
    pub async fn wait_closed(&self) {
        match &self.0 {
            Inner::Screen(s) => first_or_never(s.receive_closed().await).await,
            Inner::Remote { session, .. } => first_or_never(session.receive_closed().await).await,
        }
    }

    /// Cierra la sesión del portal: GNOME deja de compartir y apaga su
    /// indicador.
    pub async fn close(self) {
        let r = match &self.0 {
            Inner::Screen(s) => s.close().await,
            Inner::Remote { session, .. } => session.close().await,
        };
        if let Err(e) = r {
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

/// Espera el primer elemento de la señal; si no se pudo suscribir, nunca.
async fn first_or_never<S, E>(signal: Result<S, E>)
where
    S: futures_util::Stream<Item = ()>,
{
    use futures_util::StreamExt;
    match signal {
        Ok(s) => {
            let mut s = std::pin::pin!(s);
            let _ = s.next().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

/// Modo de cursor preferido entre los que ofrece el portal: incrustado en el
/// vídeo (el receptor no recibe la forma del cursor por separado desde
/// Linux); si no, oculto.
fn pick_cursor_mode(available: BitFlags<CursorMode>) -> CursorMode {
    if available.contains(CursorMode::Embedded) {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    }
}

async fn cursor_mode(sc: &Screencast<'_>) -> CursorMode {
    match sc.available_cursor_modes().await {
        Ok(modes) => pick_cursor_mode(modes),
        Err(_) => CursorMode::Embedded,
    }
}

fn is_user_refusal(e: &ashpd::Error) -> bool {
    matches!(e, ashpd::Error::Response(ResponseError::Cancelled))
}

impl PortalCaptureSession {
    /// Solicita al usuario un monitor completo y abre el PipeWire remoto. Con
    /// `with_input`, pide además el control remoto en el mismo diálogo (si el
    /// sistema no lo ofrece, sigue solo con la pantalla).
    ///
    /// Si el usuario cancela el diálogo devuelve
    /// `ashpd::Error::Response(ResponseError::Cancelled)`.
    pub async fn open(with_input: bool) -> Result<Self, ashpd::Error> {
        if with_input {
            match Self::open_remote().await {
                Ok(s) => return Ok(s),
                Err(e) if is_user_refusal(&e) => return Err(e),
                Err(e) => tracing::warn!(
                    "control remoto no disponible en el portal ({e}); solo se comparte la pantalla"
                ),
            }
        }
        Self::open_screen_only().await
    }

    async fn open_remote() -> Result<Self, ashpd::Error> {
        let rd: RemoteDesktop<'static> = RemoteDesktop::new().await?;
        let sc: Screencast<'static> = Screencast::new().await?;
        let session = rd.create_session().await?;
        let result = async {
            rd.select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
            sc.select_sources(
                &session,
                cursor_mode(&sc).await,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
            let response = rd.start(&session, None).await?.response()?;
            let stream = response
                .streams()
                .and_then(|s| s.first())
                .map(stream_info)
                .ok_or(ashpd::Error::NoResponse)?;
            let devices = response.devices();
            let pipewire_fd = sc.open_pipe_wire_remote(&session).await?;
            Ok::<_, ashpd::Error>((stream, pipewire_fd, devices))
        }
        .await;
        match result {
            Ok((stream, pipewire_fd, devices)) => {
                tracing::info!("portal: pantalla + control remoto ({devices:?})");
                Ok(Self {
                    stream,
                    pipewire_fd,
                    session: PortalSession(Inner::Remote {
                        rd,
                        session,
                        node_id: stream.node_id,
                        devices,
                    }),
                })
            }
            Err(e) => {
                // No dejar la sesión abierta (ni el indicador de GNOME).
                let _ = session.close().await;
                Err(e)
            }
        }
    }

    async fn open_screen_only() -> Result<Self, ashpd::Error> {
        let proxy: Screencast<'static> = Screencast::new().await?;
        let session = proxy.create_session().await?;
        let result = async {
            proxy
                .select_sources(
                    &session,
                    cursor_mode(&proxy).await,
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
                session: PortalSession(Inner::Screen(session)),
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
