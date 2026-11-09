//! Monitor virtual de GNOME (Mutter) para el modo **extender** en Linux.
//!
//! Usa la API D-Bus de Mutter (GNOME 40+, sesión Wayland), la misma que
//! gnome-remote-desktop:
//! - `org.gnome.Mutter.RemoteDesktop.CreateSession` (para poder inyectar
//!   ratón y teclado) y una sesión de `org.gnome.Mutter.ScreenCast` ligada a
//!   ella (`remote-desktop-session-id`);
//! - `RecordVirtual` crea el monitor virtual; Mutter publica su nodo PipeWire
//!   (señal `PipeWireStreamAdded`) y el monitor toma el tamaño que fije el
//!   consumidor al negociar el formato;
//! - `Start`/`Stop` en la sesión de control remoto (o en la de screencast si
//!   el control remoto no está disponible: entonces solo hay imagen).
//!
//! Al cerrar la sesión ([`MutterVirtual::close`]) el monitor desaparece. No
//! muestra diálogo.

use crate::linux_input::InputAction;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const SC_DEST: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SC_SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
const RD_DEST: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";
/// `cursor-mode` de Mutter: 1 = incrustado en el vídeo.
const CURSOR_EMBEDDED: u32 = 1;
/// Espera máxima a que Mutter publique el nodo PipeWire tras `Start`.
const STREAM_ADDED_TIMEOUT: Duration = Duration::from_secs(5);

/// Error al crear el monitor virtual, con un mensaje claro para el usuario.
#[derive(Debug, thiserror::Error)]
pub enum MutterError {
    #[error("extender en Linux necesita GNOME en Wayland (no se encontró org.gnome.Mutter.ScreenCast); usa «duplicar»")]
    NotAvailable,
    #[error("GNOME no creó el monitor virtual: {0}")]
    DBus(#[from] zbus::Error),
    #[error("GNOME no publicó el stream del monitor virtual en {} s", STREAM_ADDED_TIMEOUT.as_secs())]
    Timeout,
}

/// Sesión de control remoto de Mutter ligada al monitor virtual.
struct Remote {
    session: zbus::Proxy<'static>,
    /// Ruta del objeto stream (la API de entrada la usa como cadena).
    stream_path: String,
}

/// Sesión de Mutter con un monitor virtual en marcha.
pub struct MutterVirtual {
    /// Sesión que se arranca y detiene: la de control remoto si existe; si
    /// no, la de screencast.
    control: zbus::Proxy<'static>,
    remote: Option<Remote>,
    /// Nodo PipeWire del monitor virtual.
    pub node_id: u32,
}

impl std::fmt::Debug for MutterVirtual {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutterVirtual")
            .field("node_id", &self.node_id)
            .field("input", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

async fn has_owner(conn: &zbus::Connection, name: &str) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    match zbus::names::BusName::try_from(name) {
        Ok(name) => dbus.name_has_owner(name).await.unwrap_or(false),
        Err(_) => false,
    }
}

/// Crea una sesión de control remoto de Mutter y devuelve `(proxy, id)`.
async fn create_remote_session(
    conn: &zbus::Connection,
) -> Result<(zbus::Proxy<'static>, String), zbus::Error> {
    let rd = zbus::Proxy::new(conn, RD_DEST, RD_PATH, RD_DEST).await?;
    let path: OwnedObjectPath = rd.call("CreateSession", &()).await?;
    let session =
        zbus::Proxy::new_owned(conn.clone(), RD_DEST, path.into_inner(), RD_SESSION_IFACE).await?;
    let id: String = session.get_property("SessionId").await?;
    Ok((session, id))
}

impl MutterVirtual {
    /// ¿Hay un Mutter con la API de screencast en el bus de sesión?
    pub async fn is_available() -> bool {
        match zbus::Connection::session().await {
            Ok(conn) => has_owner(&conn, SC_DEST).await,
            Err(_) => false,
        }
    }

    /// Crea el monitor virtual (con control remoto si `with_input` y Mutter lo
    /// permite) y espera su nodo PipeWire. El monitor toma el tamaño que fije
    /// el consumidor al conectar a ese nodo.
    pub async fn open(with_input: bool) -> Result<Self, MutterError> {
        let conn = zbus::Connection::session().await?;
        if !has_owner(&conn, SC_DEST).await {
            return Err(MutterError::NotAvailable);
        }
        // Control remoto primero (la sesión de screencast se liga a ella).
        let remote = if with_input && has_owner(&conn, RD_DEST).await {
            match create_remote_session(&conn).await {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("control remoto de GNOME no disponible ({e}); solo imagen");
                    None
                }
            }
        } else {
            None
        };
        let mut sc_props = HashMap::<&str, Value>::new();
        if let Some((_, id)) = &remote {
            sc_props.insert("remote-desktop-session-id", Value::from(id.clone()));
        }
        let screencast = zbus::Proxy::new(&conn, SC_DEST, SC_PATH, SC_DEST).await?;
        let sc_path: OwnedObjectPath = screencast.call("CreateSession", &(sc_props,)).await?;
        let sc_session = zbus::Proxy::new_owned(
            conn.clone(),
            SC_DEST,
            sc_path.into_inner(),
            SC_SESSION_IFACE,
        )
        .await?;
        // Se arranca/detiene la de control remoto si existe (arrastra a la
        // de screencast); si no, la de screencast.
        let (control, remote_session) = match remote {
            Some((rd_session, _)) => (rd_session.clone(), Some(rd_session)),
            None => (sc_session.clone(), None),
        };
        let result = async {
            let mut props = HashMap::<&str, Value>::new();
            props.insert("cursor-mode", Value::U32(CURSOR_EMBEDDED));
            let stream_path: OwnedObjectPath = sc_session.call("RecordVirtual", &(props,)).await?;
            let stream_path_str = stream_path.as_str().to_owned();
            let stream = zbus::Proxy::new_owned(
                conn.clone(),
                SC_DEST,
                stream_path.into_inner(),
                SC_STREAM_IFACE,
            )
            .await?;
            // Suscribirse antes de Start para no perder la señal.
            let mut added = stream.receive_signal("PipeWireStreamAdded").await?;
            control.call::<_, _, ()>("Start", &()).await?;
            let msg = tokio::time::timeout(STREAM_ADDED_TIMEOUT, added.next())
                .await
                .map_err(|_| MutterError::Timeout)?
                .ok_or(MutterError::Timeout)?;
            let node_id: u32 = msg.body().deserialize()?;
            Ok::<_, MutterError>((node_id, stream_path_str))
        }
        .await;
        match result {
            Ok((node_id, stream_path)) => {
                let remote = remote_session.map(|session| Remote {
                    session,
                    stream_path,
                });
                tracing::info!(
                    "monitor virtual de GNOME listo (nodo {node_id}, control remoto: {})",
                    remote.is_some()
                );
                Ok(Self {
                    control,
                    remote,
                    node_id,
                })
            }
            Err(e) => {
                // No dejar la sesión (ni un monitor virtual) abierta.
                let _ = control.call::<_, _, ()>("Stop", &()).await;
                Err(e)
            }
        }
    }

    /// ¿Hay control remoto (ratón y teclado) en esta sesión?
    pub fn can_inject(&self) -> bool {
        self.remote.is_some()
    }

    /// Inyecta una acción de entrada. Sin control remoto no hace nada.
    pub async fn inject(&self, action: &InputAction) -> Result<(), zbus::Error> {
        let Some(r) = &self.remote else {
            return Ok(());
        };
        let s = &r.session;
        match *action {
            InputAction::PointerAbs { x, y } => {
                s.call::<_, _, ()>(
                    "NotifyPointerMotionAbsolute",
                    &(r.stream_path.as_str(), x, y),
                )
                .await
            }
            InputAction::Button { code, pressed } => {
                s.call::<_, _, ()>("NotifyPointerButton", &(code as i32, pressed))
                    .await
            }
            InputAction::Axis { horizontal, steps } => {
                s.call::<_, _, ()>("NotifyPointerAxisDiscrete", &(u32::from(horizontal), steps))
                    .await
            }
            InputAction::Key { code, pressed } => {
                s.call::<_, _, ()>("NotifyKeyboardKeycode", &(code, pressed))
                    .await
            }
        }
    }

    /// Se resuelve cuando GNOME cierra la sesión (p. ej. «Dejar de
    /// compartir» en la barra superior). Si no se puede escuchar, nunca.
    pub async fn wait_closed(&self) {
        match self.control.receive_signal("Closed").await {
            Ok(mut closed) => {
                let _ = closed.next().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }

    /// Detiene la sesión: el monitor virtual desaparece de GNOME.
    pub async fn close(self) {
        if let Err(e) = self.control.call::<_, _, ()>("Stop", &()).await {
            tracing::debug!("deteniendo la sesión de Mutter: {e}");
        }
    }
}
