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

pub const IPC_VERSION: IpcVersion = IpcVersion { major: 1, minor: 1 };

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
    /// Inicia una sesión emisora hacia un receptor (`crossscreen-viewer --listen`).
    /// `addr` = `IP:puerto`, `peer` = huella completa o prefijo de un par de confianza.
    StartStream {
        addr: String,
        peer: String,
        mode: StreamModeInfo,
        /// Resolución y refresco pedidos; `None` usa el modo nativo anunciado por el receptor.
        mode_override: Option<ModeInfo>,
        /// Tasa objetivo en kbps (p. ej. 20000).
        bitrate_kbps: u32,
        /// Perfil de red (ajustes de FEC, *pacing* y tasa por defecto).
        #[serde(default)]
        profile: StreamProfileInfo,
        /// Sincronizar el portapapeles de texto (F7). Desactivado por defecto.
        #[serde(default)]
        clipboard: bool,
    },
    /// Detiene la sesión emisora en curso (si la hay) y retira el monitor virtual.
    StopStream,
    /// Inicia el visor receptor (`crossscreen-viewer --listen`) en este equipo.
    StartReceiver {
        port: u16,
        fullscreen: bool,
        /// Sincronizar el portapapeles de texto (F7). Desactivado por defecto.
        #[serde(default)]
        clipboard: bool,
    },
    /// Detiene el visor receptor en curso (si lo hay).
    StopReceiver,
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
    StreamStarted { session: StreamSessionInfo },
    ReceiverStarted { receiver: ReceiverSessionInfo },
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
    StreamStarted { session: StreamSessionInfo },
    StreamEnded { report: Box<StreamReportInfo> },
    ReceiverStarted { receiver: ReceiverSessionInfo },
    ReceiverEnded { message: Option<String> },
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
    pub streaming_supported: bool,
    pub active_stream: Option<StreamSessionInfo>,
    pub last_stream: Option<Box<StreamReportInfo>>,
    pub active_receiver: Option<ReceiverSessionInfo>,
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
pub enum StreamModeInfo {
    Extend,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfileInfo {
    /// Equilibrado con escalado automático por resolución (por defecto).
    #[default]
    Auto,
    /// Wi-Fi con mayor paridad FEC inicial y *pacing* más suave ante ráfagas.
    Wifi,
    /// Cable Ethernet: mayor tasa de bits, menor sobrecarga FEC y menor ventana de *pacing*.
    Lan,
    /// Ahorro de ancho de banda para enlaces congestionados o débiles.
    Saver,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSessionInfo {
    pub peer_name: String,
    pub peer_fingerprint: String,
    pub peer_fingerprint_short: String,
    pub remote_addr: String,
    pub mode: StreamModeInfo,
    pub mode_override: Option<ModeInfo>,
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub profile: StreamProfileInfo,
    /// RTT inicial al conectar (µs).
    pub initial_rtt_us: u64,
    /// Segundos desde la época Unix en que se inició la sesión.
    pub started_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySummary {
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub samples: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReportInfo {
    pub peer_name: String,
    pub remote_addr: String,
    pub duration_ms: u64,
    pub end_reason: String,
    pub captured_size: Option<(u32, u32)>,
    pub frames_captured: u64,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub dropped_before_encode: u64,
    pub dropped_after_encode: u64,
    pub idr_sent: u64,
    pub keyframe_requests: u64,
    #[serde(default)]
    pub reconnections: u64,
    #[serde(default)]
    pub input_events_injected: u64,
    pub payload_bytes: u64,
    pub wire_bytes: u64,
    pub datagrams_sent: u64,
    pub datagrams_failed: u64,
    pub cursor_positions: u64,
    pub cursor_shapes: u64,
    pub capture_us: Option<LatencySummary>,
    pub encode_us: Option<LatencySummary>,
    pub capture_to_sent_us: Option<LatencySummary>,
    pub receiver_frames_received: Option<u64>,
    pub receiver_frames_lost: Option<u64>,
    pub receiver_jitter_us: Option<u32>,
    pub receiver_decode_us: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverSessionInfo {
    pub listen_addr: String,
    pub port: u16,
    pub fullscreen: bool,
    pub pid: u32,
    pub started_at: u64,
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
            Request::StartStream {
                addr: "192.168.1.127:47810".into(),
                peer: "ff81".into(),
                mode: StreamModeInfo::Extend,
                mode_override: Some(ModeInfo {
                    width: 1920,
                    height: 1080,
                    refresh_mhz: 60_000,
                }),
                bitrate_kbps: 20_000,
                profile: StreamProfileInfo::Wifi,
                clipboard: true,
            },
            Request::StopStream,
            Request::StartReceiver {
                port: 47810,
                fullscreen: true,
                clipboard: false,
            },
            Request::StopReceiver,
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
            AgentMessage::Response {
                id: 11,
                result: Ok(Response::StreamStarted {
                    session: StreamSessionInfo {
                        peer_name: "DESKTOP-93VFE2P".into(),
                        peer_fingerprint: "ff".repeat(32),
                        peer_fingerprint_short: "ffff-ffff-ffff-ffff".into(),
                        remote_addr: "192.168.1.127:47810".into(),
                        mode: StreamModeInfo::Extend,
                        mode_override: None,
                        bitrate_kbps: 20_000,
                        profile: StreamProfileInfo::Auto,
                        initial_rtt_us: 12_500,
                        started_at: 1_700_000_100,
                    },
                }),
            },
            AgentMessage::Event {
                event: Event::StreamEnded {
                    report: Box::new(StreamReportInfo {
                        peer_name: "DESKTOP-93VFE2P".into(),
                        remote_addr: "192.168.1.127:47810".into(),
                        duration_ms: 60_000,
                        end_reason: "detenido por el usuario".into(),
                        captured_size: Some((1366, 768)),
                        frames_captured: 1825,
                        frames_encoded: 1781,
                        frames_sent: 1781,
                        dropped_before_encode: 44,
                        dropped_after_encode: 0,
                        idr_sent: 4,
                        keyframe_requests: 1,
                        reconnections: 0,
                        input_events_injected: 0,
                        payload_bytes: 30_000_000,
                        wire_bytes: 34_000_000,
                        datagrams_sent: 25_382,
                        datagrams_failed: 0,
                        cursor_positions: 3040,
                        cursor_shapes: 60,
                        capture_us: Some(LatencySummary {
                            p50_us: 240,
                            p95_us: 400,
                            max_us: 17_650,
                            samples: 1825,
                        }),
                        encode_us: Some(LatencySummary {
                            p50_us: 6910,
                            p95_us: 9920,
                            max_us: 25_300,
                            samples: 1781,
                        }),
                        capture_to_sent_us: Some(LatencySummary {
                            p50_us: 18_130,
                            p95_us: 25_620,
                            max_us: 53_180,
                            samples: 1781,
                        }),
                        receiver_frames_received: Some(1776),
                        receiver_frames_lost: Some(2),
                        receiver_jitter_us: Some(10_663),
                        receiver_decode_us: Some(681),
                        error: None,
                    }),
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
            streaming_supported: true,
            active_stream: None,
            last_stream: None,
            active_receiver: None,
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
