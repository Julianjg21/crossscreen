//! Mensajes del protocolo UI ⇄ agente.
//!
//! Codificación: JSON, un mensaje por línea (`\n`). Se eligió JSON en lugar de
//! postcard porque la UI web (TypeScript) consume los mismos tipos a través de
//! Tauri y así se depura con herramientas estándar. El tráfico es mínimo.
//!
//! Secuencia:
//! 1. El cliente envía [`ClientMessage::Hello`] con su versión de protocolo.
//! 2. El agente responde [`AgentMessage::Welcome`] (o
//!    [`AgentMessage::Rejected`] si la versión mayor no coincide y cierra).
//! 3. El cliente envía [`ClientMessage::Request`] con un `id` propio; el
//!    agente contesta con [`AgentMessage::Response`] con el mismo `id`. Las
//!    peticiones pueden solaparse (p. ej. `PairWith` tarda varios segundos).
//! 4. En cualquier momento el agente puede emitir [`AgentMessage::Event`]
//!    (se difunden a todos los clientes conectados).

use serde::{Deserialize, Serialize};

/// Versión del protocolo IPC. Un cambio incompatible incrementa `major`;
/// añadir variantes o campos opcionales incrementa `minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcVersion {
    pub major: u16,
    pub minor: u16,
}

pub const IPC_VERSION: IpcVersion = IpcVersion { major: 1, minor: 0 };

/// Tamaño máximo de una línea (mensaje). Protege al agente frente a clientes
/// que envían datos sin fin.
pub const MAX_LINE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: IpcVersion,
        /// Nombre del cliente, solo para el registro (p. ej. "desktop", "probe").
        client: String,
    },
    Request {
        id: u64,
        request: Request,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    Welcome {
        version: IpcVersion,
        agent_version: String,
    },
    Rejected {
        reason: String,
    },
    Response {
        id: u64,
        result: Result<Response, IpcError>,
    },
    Event {
        event: Event,
    },
}

/// Peticiones de la UI al agente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    GetStatus,
    GetIdentity,
    ListPeers,
    /// Huella completa (64 hex) del par a olvidar.
    ForgetPeer {
        fingerprint: String,
    },
    /// Abre un listener de emparejamiento, genera un PIN y espera (2 min).
    /// Responde `PairingStarted`; el resultado llega como evento `PairingResult`.
    StartPairingListener,
    CancelPairing,
    /// Se empareja con un equipo que muestra un PIN. `addr` = `IP:puerto`.
    PairWith {
        addr: String,
        pin: String,
    },
    /// Empieza a explorar la red local (idempotente). Responde con los
    /// equipos ya conocidos; los cambios llegan como eventos.
    StartDiscovery,
    ListDisplays,
    ShowDisplay {
        width: u32,
        height: u32,
        hz: u32,
    },
    HideDisplays,
}

/// Respuestas del agente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Status(AgentStatus),
    Identity(IdentityInfo),
    Peers { peers: Vec<PeerInfo> },
    Ok,
    PairingStarted { pin: String, port: u16 },
    Paired { peer: PeerInfo },
    Discovered { devices: Vec<DiscoveredDevice> },
    Displays { displays: Vec<DisplayInfo> },
    DisplayShown { gdi_name: String, mode: ModeInfo },
    DisplaysHidden { count: u32 },
}

/// Eventos que el agente difunde a los clientes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    PairingPin { pin: String, port: u16 },
    PairingResult(PairingOutcome),
    PeersChanged,
    DeviceDiscovered(DiscoveredDevice),
    DeviceLost { fullname: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PairingOutcome {
    Paired { peer: PeerInfo },
    Failed { message: String },
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent_version: String,
    pub ipc_version: IpcVersion,
    /// `windows`, `linux`, …
    pub platform: String,
    /// Puerto UDP del listener de emparejamiento si está activo.
    pub pairing_port: Option<u16>,
    pub discovery_active: bool,
    /// `false` si el servicio mDNS no pudo arrancar.
    pub mdns_available: bool,
    pub displays_supported: bool,
    pub data_dir: String,
    pub log_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityInfo {
    pub name: String,
    pub device_id: String,
    /// Huella SHA-256 completa del certificado (64 hex).
    pub fingerprint: String,
    pub fingerprint_short: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    /// Segundos desde la época Unix (0 si se desconoce).
    pub paired_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    /// Nombre completo del servicio mDNS (clave para `DeviceLost`).
    pub fullname: String,
    pub device_id: String,
    pub name: String,
    pub protocol_major: u16,
    pub roles: String,
    pub addresses: Vec<String>,
    pub session_port: u16,
    pub pairing_port: Option<u16>,
    /// Si su device id coincide con un par de confianza.
    pub paired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub friendly_name: String,
    pub device_path: String,
    pub is_virtual: bool,
    pub active: bool,
    pub gdi_name: Option<String>,
    /// Modos anunciados (solo para monitores virtuales activos).
    pub modes: Vec<ModeInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Busy,
    Unsupported,
    Timeout,
    Failed,
}

/// Error devuelto por el agente para una petición concreta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct IpcError {
    pub code: ErrorCode,
    pub message: String,
}

impl IpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        IpcError {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(v: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let s = serde_json::to_string(v).unwrap();
        assert!(!s.contains('\n'), "una línea por mensaje");
        let back: T = serde_json::from_str(&s).unwrap();
        assert_eq!(&back, v);
    }

    fn peer() -> PeerInfo {
        PeerInfo {
            name: "portátil \"B\"".into(),
            fingerprint: "ab".repeat(32),
            fingerprint_short: "abab-abab-abab-abab".into(),
            paired_at: 1_700_000_000,
        }
    }

    #[test]
    fn all_requests_roundtrip() {
        let reqs = [
            Request::GetStatus,
            Request::GetIdentity,
            Request::ListPeers,
            Request::ForgetPeer {
                fingerprint: "00".repeat(32),
            },
            Request::StartPairingListener,
            Request::CancelPairing,
            Request::PairWith {
                addr: "192.168.1.20:50000".into(),
                pin: "123456".into(),
            },
            Request::StartDiscovery,
            Request::ListDisplays,
            Request::ShowDisplay {
                width: 1920,
                height: 1080,
                hz: 60,
            },
            Request::HideDisplays,
        ];
        for (i, r) in reqs.into_iter().enumerate() {
            roundtrip(&ClientMessage::Request {
                id: i as u64,
                request: r,
            });
        }
        roundtrip(&ClientMessage::Hello {
            version: IPC_VERSION,
            client: "test".into(),
        });
    }

    #[test]
    fn responses_and_events_roundtrip() {
        let dev = DiscoveredDevice {
            fullname: "cs-x._crossscreen._udp.local.".into(),
            device_id: "11".repeat(16),
            name: "Sobremesa".into(),
            protocol_major: 0,
            roles: "source,sink".into(),
            addresses: vec!["192.168.1.2".into(), "fe80::1".into()],
            session_port: 0,
            pairing_port: Some(5000),
            paired: false,
        };
        let msgs = [
            AgentMessage::Welcome {
                version: IPC_VERSION,
                agent_version: "0.0.1".into(),
            },
            AgentMessage::Rejected {
                reason: "versión".into(),
            },
            AgentMessage::Response {
                id: 7,
                result: Ok(Response::Peers {
                    peers: vec![peer()],
                }),
            },
            AgentMessage::Response {
                id: 8,
                result: Err(IpcError::new(ErrorCode::NotFound, "no existe")),
            },
            AgentMessage::Response {
                id: 9,
                result: Ok(Response::Displays {
                    displays: vec![DisplayInfo {
                        friendly_name: "VDD by MTT".into(),
                        device_path: r"\\?\DISPLAY#MTT1337#".into(),
                        is_virtual: true,
                        active: true,
                        gdi_name: Some(r"\\.\DISPLAY3".into()),
                        modes: vec![ModeInfo {
                            width: 1920,
                            height: 1080,
                            refresh_mhz: 60_000,
                        }],
                    }],
                }),
            },
            AgentMessage::Response {
                id: 10,
                result: Ok(Response::Ok),
            },
            AgentMessage::Event {
                event: Event::PairingPin {
                    pin: "000123".into(),
                    port: 1234,
                },
            },
            AgentMessage::Event {
                event: Event::PairingResult(PairingOutcome::Paired { peer: peer() }),
            },
            AgentMessage::Event {
                event: Event::PairingResult(PairingOutcome::Failed {
                    message: "PIN incorrecto".into(),
                }),
            },
            AgentMessage::Event {
                event: Event::DeviceDiscovered(dev),
            },
            AgentMessage::Event {
                event: Event::DeviceLost {
                    fullname: "x".into(),
                },
            },
        ];
        for m in &msgs {
            roundtrip(m);
        }
    }

    #[test]
    fn wire_format_is_stable() {
        // La UI TypeScript depende de esta forma exacta.
        let s = serde_json::to_string(&ClientMessage::Request {
            id: 1,
            request: Request::ShowDisplay {
                width: 1920,
                height: 1080,
                hz: 60,
            },
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"type":"request","id":1,"request":{"kind":"show_display","width":1920,"height":1080,"hz":60}}"#
        );
        let s = serde_json::to_string(&Response::Status(AgentStatus {
            agent_version: "1".into(),
            ipc_version: IPC_VERSION,
            platform: "windows".into(),
            pairing_port: None,
            discovery_active: false,
            mdns_available: true,
            displays_supported: true,
            data_dir: "d".into(),
            log_file: None,
        }))
        .unwrap();
        assert!(s.starts_with(r#"{"kind":"status","agent_version":"1""#));
    }

    #[test]
    fn unknown_request_is_an_error_not_a_panic() {
        let r: Result<ClientMessage, _> =
            serde_json::from_str(r#"{"type":"request","id":1,"request":{"kind":"format_disk"}}"#);
        assert!(r.is_err());
    }
}
