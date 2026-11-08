//! Transporte en Unix: socket de dominio Unix en `$XDG_RUNTIME_DIR`.
//!
//! **Estado: solo compilado, NO probado en Linux todavía.**
//!
//! - El socket vive en `$XDG_RUNTIME_DIR/crossscreen/agent.sock`; el
//!   directorio se crea con permisos `0700` y se comprueba que pertenece al
//!   usuario y que no es accesible por otros.
//! - Cada conexión se verifica con las credenciales del par (`SO_PEERCRED`
//!   vía `UnixStream::peer_cred`): solo se acepta el mismo UID que posee el
//!   directorio (el nuestro).
//! - Instancia única: si el socket existe y responde, otro agente está en
//!   ejecución; si existe pero no responde, se considera huérfano y se borra.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

pub type ServerStream = UnixStream;
pub type ClientStream = UnixStream;

pub fn default_socket_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR no está definido; no hay un directorio privado para el socket",
        )
    })?;
    Ok(PathBuf::from(base).join("crossscreen").join("agent.sock"))
}

fn ensure_private_dir(dir: &Path) -> io::Result<u32> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let meta = std::fs::metadata(dir)?;
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "el directorio del socket es accesible por otros usuarios",
        ));
    }
    Ok(meta.uid())
}

#[derive(Debug)]
pub struct Listener {
    inner: UnixListener,
    uid: u32,
}

/// Indica si el error significa que ya hay otro agente escuchando.
pub fn is_name_in_use(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AddrInUse
}

pub async fn bind(path: &Path) -> io::Result<Listener> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ruta de socket sin directorio")
    })?;
    let uid = ensure_private_dir(dir)?;
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "ya hay un agente escuchando en el socket",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let inner = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener { inner, uid })
}

impl Listener {
    /// Acepta la siguiente conexión de nuestro propio UID (las demás se cierran).
    pub async fn accept(&self) -> io::Result<UnixStream> {
        loop {
            let (stream, _) = self.inner.accept().await?;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.uid => return Ok(stream),
                Ok(cred) => {
                    tracing::warn!(uid = cred.uid(), "conexión IPC de otro usuario rechazada")
                }
                Err(e) => tracing::warn!("no se pudieron leer las credenciales del par: {e}"),
            }
        }
    }
}

pub async fn connect_client(path: &Path) -> io::Result<UnixStream> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ruta de socket sin directorio")
    })?;
    let my_uid = std::fs::metadata(dir)?.uid();
    let stream = UnixStream::connect(path).await?;
    // El servidor también debe ser de nuestro UID.
    let cred = stream.peer_cred()?;
    if cred.uid() != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "el socket del agente pertenece a otro usuario",
        ));
    }
    Ok(stream)
}
