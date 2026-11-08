//! IPC local entre la UI (`apps/desktop`) y el agente (`crossscreen-agent`).
//!
//! - [`protocol`]: mensajes versionados (JSON por líneas).
//! - [`transport`]: named pipe con DACL del usuario actual en Windows; socket
//!   Unix `0700` + `SO_PEERCRED` en Linux (este último solo compilado, sin
//!   probar todavía).
//! - [`client`] / [`server`]: multiplexación de peticiones, respuestas y eventos.

pub mod client;
mod framing;
pub mod protocol;
pub mod server;
pub mod transport;

pub use client::IpcClient;
pub use protocol::*;
pub use server::{serve_connection, Handler, HandlerFuture};
pub use transport::{Endpoint, Listener};

/// Errores del lado cliente (o de transporte).
#[derive(Debug, thiserror::Error)]
pub enum IpcClientError {
    #[error("E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocolo IPC: {0}")]
    Protocol(String),
    #[error("versión de protocolo incompatible: {0}")]
    VersionMismatch(String),
    #[error("el agente cerró la conexión")]
    Disconnected,
    #[error("{0}")]
    Agent(IpcError),
}
