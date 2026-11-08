//! Transporte local por plataforma con una API común.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::io;

#[cfg(windows)]
pub use self::windows::current_user_sid;

/// Dirección local del agente: nombre de tubería (Windows) o ruta de socket (Unix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    /// Dirección por defecto del agente del usuario actual.
    pub fn default_for_user() -> io::Result<Self> {
        #[cfg(windows)]
        return Ok(Endpoint(windows::default_pipe_name()?));
        #[cfg(unix)]
        return Ok(Endpoint(
            unix::default_socket_path()?.to_string_lossy().into_owned(),
        ));
    }

    /// Dirección arbitraria (tests). En Windows debe empezar por `\\.\pipe\`.
    pub fn custom(s: impl Into<String>) -> Self {
        Endpoint(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(windows)]
pub type ServerStream = windows::ServerStream;
#[cfg(unix)]
pub type ServerStream = unix::ServerStream;
#[cfg(windows)]
pub type ClientStream = windows::ClientStream;
#[cfg(unix)]
pub type ClientStream = unix::ClientStream;

/// Indica si un error de [`Listener::bind`] significa que ya hay otro agente.
pub fn is_name_in_use(e: &io::Error) -> bool {
    #[cfg(windows)]
    return windows::is_name_in_use(e);
    #[cfg(unix)]
    return unix::is_name_in_use(e);
}

/// Escucha conexiones locales autenticadas.
#[derive(Debug)]
pub struct Listener {
    #[cfg(windows)]
    name: String,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(unix)]
    inner: unix::Listener,
}

impl Listener {
    /// Crea el punto de escucha. Falla si ya existe (instancia única); usa
    /// [`is_name_in_use`] para distinguir ese caso.
    pub async fn bind(ep: &Endpoint) -> io::Result<Self> {
        #[cfg(windows)]
        return Ok(Listener {
            name: ep.0.clone(),
            next: windows::create_server(&ep.0, true)?,
        });
        #[cfg(unix)]
        return Ok(Listener {
            inner: unix::bind(std::path::Path::new(&ep.0)).await?,
        });
    }

    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        #[cfg(windows)]
        {
            self.next.connect().await?;
            let fresh = windows::create_server(&self.name, false)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
        #[cfg(unix)]
        self.inner.accept().await
    }
}

/// Conecta con el agente verificando que pertenece al usuario actual.
pub async fn connect(ep: &Endpoint) -> io::Result<ClientStream> {
    #[cfg(windows)]
    return windows::connect_client(&ep.0).await;
    #[cfg(unix)]
    return unix::connect_client(std::path::Path::new(&ep.0)).await;
}
