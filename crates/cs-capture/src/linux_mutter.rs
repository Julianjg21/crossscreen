//! Monitor virtual de GNOME (Mutter) para el modo **extender** en Linux.
//!
//! Usa la API D-Bus de Mutter `org.gnome.Mutter.ScreenCast` (GNOME 40+,
//! sesión Wayland): `CreateSession` → `RecordVirtual` → `Start`. Mutter
//! publica un nodo PipeWire (señal `PipeWireStreamAdded`) y crea un monitor
//! virtual cuyo tamaño es el que el consumidor fija al negociar el formato
//! de PipeWire. Al cerrar la sesión ([`MutterVirtual::close`]) el monitor
//! desaparece. No muestra diálogo: es la misma API que usa
//! gnome-remote-desktop.

use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const DEST: &str = "org.gnome.Mutter.ScreenCast";
const PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
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

/// Sesión de Mutter con un monitor virtual en marcha.
pub struct MutterVirtual {
    session: zbus::Proxy<'static>,
    /// Nodo PipeWire del monitor virtual.
    pub node_id: u32,
}

impl std::fmt::Debug for MutterVirtual {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutterVirtual")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl MutterVirtual {
    /// ¿Hay un Mutter con la API de screencast en el bus de sesión?
    pub async fn is_available() -> bool {
        let Ok(conn) = zbus::Connection::session().await else {
            return false;
        };
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
            return false;
        };
        match zbus::names::BusName::try_from(DEST) {
            Ok(name) => dbus.name_has_owner(name).await.unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Crea la sesión, el monitor virtual y espera su nodo PipeWire. El
    /// monitor toma el tamaño que fije el consumidor al conectar a ese nodo.
    pub async fn open() -> Result<Self, MutterError> {
        if !Self::is_available().await {
            return Err(MutterError::NotAvailable);
        }
        let conn = zbus::Connection::session().await?;
        let screencast = zbus::Proxy::new(&conn, DEST, PATH, DEST).await?;
        let session_path: OwnedObjectPath = screencast
            .call("CreateSession", &(HashMap::<&str, Value>::new(),))
            .await?;
        let session =
            zbus::Proxy::new_owned(conn.clone(), DEST, session_path.into_inner(), SESSION_IFACE)
                .await?;
        let result = async {
            let mut props = HashMap::<&str, Value>::new();
            props.insert("cursor-mode", Value::U32(CURSOR_EMBEDDED));
            let stream_path: OwnedObjectPath = session.call("RecordVirtual", &(props,)).await?;
            let stream =
                zbus::Proxy::new_owned(conn.clone(), DEST, stream_path.into_inner(), STREAM_IFACE)
                    .await?;
            // Suscribirse antes de Start para no perder la señal.
            let mut added = stream.receive_signal("PipeWireStreamAdded").await?;
            session.call::<_, _, ()>("Start", &()).await?;
            let msg = tokio::time::timeout(STREAM_ADDED_TIMEOUT, added.next())
                .await
                .map_err(|_| MutterError::Timeout)?
                .ok_or(MutterError::Timeout)?;
            let node_id: u32 = msg.body().deserialize()?;
            Ok::<_, MutterError>(node_id)
        }
        .await;
        match result {
            Ok(node_id) => Ok(Self { session, node_id }),
            Err(e) => {
                // No dejar la sesión (ni un monitor virtual) abierta.
                let _ = session.call::<_, _, ()>("Stop", &()).await;
                Err(e)
            }
        }
    }

    /// Se resuelve cuando GNOME cierra la sesión (p. ej. «Dejar de
    /// compartir» en la barra superior). Si no se puede escuchar, nunca.
    pub async fn wait_closed(&self) {
        match self.session.receive_signal("Closed").await {
            Ok(mut closed) => {
                let _ = closed.next().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }

    /// Detiene la sesión: el monitor virtual desaparece de GNOME.
    pub async fn close(self) {
        if let Err(e) = self.session.call::<_, _, ()>("Stop", &()).await {
            tracing::debug!("deteniendo la sesión de Mutter: {e}");
        }
    }
}
