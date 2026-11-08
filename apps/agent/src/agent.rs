//! Estado y lógica del agente. Todo lo que se expone por IPC es funcionalidad
//! real del núcleo: no hay datos simulados.

use anyhow::{Context, Result};
use cs_core::Roles;
use cs_discovery::{Announcement, DiscoveredPeer, Discovery, DiscoveryEvent};
use cs_ipc::{
    AgentStatus, DiscoveredDevice, DisplayInfo, ErrorCode, Event, Handler, HandlerFuture,
    IdentityInfo, IpcError, ModeInfo, PairingOutcome, PeerInfo, Request, Response, IPC_VERSION,
};
use cs_protocol::{ALPN_PAIRING, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::pairing::Pin;
use cs_security::ratelimit::AttemptLimiter;
use cs_security::trust::{PeerRecord, TrustStore};
use cs_security::verifier::PeerPolicy;
use cs_transport::pairing::{pair_as_initiator, pair_as_responder};
use cs_transport::Endpoint;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

/// Validez del PIN mostrado.
const PAIRING_LISTEN_TIMEOUT: Duration = Duration::from_secs(120);
/// Tiempo máximo para emparejar con un equipo que muestra un PIN.
const PAIR_WITH_TIMEOUT: Duration = Duration::from_secs(30);

struct PairingListener {
    pin: String,
    port: u16,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct DiscoveryState {
    active: bool,
    devices: BTreeMap<String, DiscoveredDevice>,
}

pub struct Agent {
    dir: PathBuf,
    name: String,
    identity: Identity,
    log_file: Option<PathBuf>,
    events: broadcast::Sender<Event>,
    /// Serializa las escrituras del agente sobre `trusted-peers.json`. El
    /// archivo se relee en cada operación para ver los cambios de la CLI.
    trust_lock: tokio::sync::Mutex<()>,
    /// `None` si el servicio mDNS no pudo arrancar.
    mdns: Option<Mutex<Discovery>>,
    pairing: tokio::sync::Mutex<Option<PairingListener>>,
    discovery: Mutex<DiscoveryState>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("dir", &self.dir)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn err(code: ErrorCode, msg: impl Into<String>) -> IpcError {
    IpcError::new(code, msg)
}

fn failed(e: impl std::fmt::Display) -> IpcError {
    err(ErrorCode::Failed, e.to_string())
}

fn peer_info(p: &PeerRecord) -> PeerInfo {
    PeerInfo {
        name: p.name.clone(),
        fingerprint: p.fingerprint.to_string(),
        fingerprint_short: p.fingerprint.short(),
        paired_at: p.paired_at,
    }
}

impl Agent {
    pub fn start(dir: PathBuf, name: String, log_file: Option<PathBuf>) -> Result<Arc<Self>> {
        let identity = Identity::load_or_create(&dir).context("cargando la identidad")?;
        let mdns = match Discovery::new() {
            Ok(d) => Some(Mutex::new(d)),
            Err(e) => {
                tracing::warn!("mDNS no disponible: {e}");
                None
            }
        };
        let (events, _) = broadcast::channel(256);
        let agent = Arc::new(Agent {
            dir,
            name,
            identity,
            log_file,
            events,
            trust_lock: tokio::sync::Mutex::new(()),
            mdns,
            pairing: tokio::sync::Mutex::new(None),
            discovery: Mutex::new(DiscoveryState::default()),
        });
        agent.announce(None);
        Ok(agent)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn shutdown(&self) {
        if let Some(m) = &self.mdns {
            if let Ok(mut d) = m.lock() {
                let _ = d.withdraw();
            }
        }
    }

    fn emit(&self, e: Event) {
        // Sin clientes conectados no es un error.
        let _ = self.events.send(e);
    }

    /// Anuncia este equipo por mDNS. Aún no hay sesiones de vídeo, así que
    /// `session_port` es 0; mientras se muestra un PIN se anuncia su puerto.
    fn announce(&self, pairing_port: Option<u16>) {
        let Some(m) = &self.mdns else { return };
        let a = Announcement {
            device_id: self.identity.fingerprint().device_id(),
            name: self.name.clone(),
            roles: Roles::BOTH,
            protocol_major: PROTOCOL_VERSION.major,
            session_port: 0,
            pairing_port,
        };
        match m.lock() {
            Ok(mut d) => {
                if let Err(e) = d.announce(&a) {
                    tracing::warn!("no se pudo anunciar por mDNS: {e}");
                }
            }
            Err(_) => tracing::error!("estado mDNS envenenado"),
        }
    }

    fn trust(&self) -> Result<TrustStore, IpcError> {
        TrustStore::open(self.dir.join("trusted-peers.json"))
            .map_err(|e| failed(format!("almacén de confianza: {e}")))
    }

    async fn add_trusted(&self, fp: cs_security::Fingerprint, name: &str) -> Result<(), IpcError> {
        let _g = self.trust_lock.lock().await;
        self.trust()?
            .add(fp, name)
            .map_err(|e| failed(format!("guardando el par: {e}")))
    }

    async fn dispatch(self: Arc<Self>, req: Request) -> Result<Response, IpcError> {
        match req {
            Request::GetStatus => Ok(Response::Status(self.status().await)),
            Request::GetIdentity => {
                let fp = self.identity.fingerprint();
                Ok(Response::Identity(IdentityInfo {
                    name: self.name.clone(),
                    device_id: fp.device_id().to_string(),
                    fingerprint: fp.to_string(),
                    fingerprint_short: fp.short(),
                }))
            }
            Request::ListPeers => {
                let _g = self.trust_lock.lock().await;
                let trust = self.trust()?;
                Ok(Response::Peers {
                    peers: trust.peers().map(peer_info).collect(),
                })
            }
            Request::ForgetPeer { fingerprint } => self.forget(&fingerprint).await,
            Request::StartPairingListener => self.start_pairing().await,
            Request::CancelPairing => {
                let mut slot = self.pairing.lock().await;
                match slot.as_mut().and_then(|p| p.cancel.take()) {
                    Some(c) => {
                        let _ = c.send(());
                        Ok(Response::Ok)
                    }
                    None => Err(err(
                        ErrorCode::NotFound,
                        "No hay ningún emparejamiento en curso",
                    )),
                }
            }
            Request::PairWith { addr, pin } => self.pair_with(&addr, &pin).await,
            Request::StartDiscovery => self.start_discovery(),
            Request::ListDisplays => displays::list().await,
            Request::ShowDisplay { width, height, hz } => displays::show(width, height, hz).await,
            Request::HideDisplays => displays::hide().await,
        }
    }

    async fn status(&self) -> AgentStatus {
        let pairing_port = self.pairing.lock().await.as_ref().map(|p| p.port);
        let discovery_active = self.discovery.lock().map(|d| d.active).unwrap_or(false);
        AgentStatus {
            agent_version: env!("CARGO_PKG_VERSION").into(),
            ipc_version: IPC_VERSION,
            platform: std::env::consts::OS.into(),
            pairing_port,
            discovery_active,
            mdns_available: self.mdns.is_some(),
            displays_supported: cfg!(windows),
            data_dir: self.dir.display().to_string(),
            log_file: self.log_file.as_ref().map(|p| p.display().to_string()),
        }
    }

    async fn forget(&self, fingerprint: &str) -> Result<Response, IpcError> {
        let wanted = fingerprint.trim().to_lowercase();
        let _g = self.trust_lock.lock().await;
        let mut trust = self.trust()?;
        let Some(fp) = trust
            .fingerprints()
            .into_iter()
            .find(|fp| fp.to_string() == wanted)
        else {
            return Err(err(ErrorCode::NotFound, "Ese equipo no está emparejado"));
        };
        trust
            .remove(&fp)
            .map_err(|e| failed(format!("guardando el almacén: {e}")))?;
        tracing::info!(peer = %fp.short(), "par olvidado");
        self.emit(Event::PeersChanged);
        Ok(Response::Ok)
    }

    async fn start_pairing(self: &Arc<Self>) -> Result<Response, IpcError> {
        let mut slot = self.pairing.lock().await;
        if let Some(p) = slot.as_ref() {
            // Ya hay un PIN activo: devolverlo (p. ej. la UI se reabrió).
            return Ok(Response::PairingStarted {
                pin: p.pin.clone(),
                port: p.port,
            });
        }
        let bind: SocketAddr = ([0, 0, 0, 0], 0).into();
        let endpoint = Endpoint::server(
            bind,
            self.identity.clone(),
            PeerPolicy::AnyForPairing,
            ALPN_PAIRING,
        )
        .map_err(|e| failed(format!("abriendo el puerto de emparejamiento: {e}")))?;
        let port = endpoint
            .local_addr()
            .map_err(|e| failed(format!("puerto de emparejamiento: {e}")))?
            .port();
        let pin = Pin::generate();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        *slot = Some(PairingListener {
            pin: pin.as_str().to_owned(),
            port,
            cancel: Some(cancel_tx),
        });
        drop(slot);
        self.announce(Some(port));
        tracing::info!(port, "emparejamiento: esperando (PIN visible en la UI)");
        self.emit(Event::PairingPin {
            pin: pin.as_str().to_owned(),
            port,
        });

        let agent = self.clone();
        let pin_out = pin.as_str().to_owned();
        tokio::spawn(async move {
            let mut limiter = AttemptLimiter::default();
            let attempt = tokio::time::timeout(
                PAIRING_LISTEN_TIMEOUT,
                pair_as_responder(&endpoint, &pin, &agent.name, &mut limiter),
            );
            let outcome = tokio::select! {
                r = attempt => match r {
                    Ok(Ok(peer)) => match agent.add_trusted(peer.fingerprint, &peer.name).await {
                        Ok(()) => {
                            tracing::info!(peer = %peer.fingerprint.short(), "emparejado (responder)");
                            PairingOutcome::Paired {
                                peer: PeerInfo {
                                    name: peer.name.clone(),
                                    fingerprint: peer.fingerprint.to_string(),
                                    fingerprint_short: peer.fingerprint.short(),
                                    paired_at: 0,
                                },
                            }
                        }
                        Err(e) => PairingOutcome::Failed { message: e.message },
                    },
                    Ok(Err(e)) => PairingOutcome::Failed { message: e.to_string() },
                    Err(_) => PairingOutcome::Expired,
                },
                _ = cancel_rx => PairingOutcome::Cancelled,
            };
            endpoint.close();
            *agent.pairing.lock().await = None;
            agent.announce(None);
            if matches!(outcome, PairingOutcome::Paired { .. }) {
                agent.emit(Event::PeersChanged);
            }
            tracing::info!(?outcome, "emparejamiento terminado");
            agent.emit(Event::PairingResult(outcome));
        });
        Ok(Response::PairingStarted { pin: pin_out, port })
    }

    async fn pair_with(&self, addr: &str, pin: &str) -> Result<Response, IpcError> {
        let addr: SocketAddr = addr.trim().parse().map_err(|_| {
            err(
                ErrorCode::BadRequest,
                "Dirección inválida: usa IP:puerto (p. ej. 192.168.1.20:50000)",
            )
        })?;
        let pin = Pin::parse(pin)
            .map_err(|_| err(ErrorCode::BadRequest, "El PIN debe tener 6 dígitos"))?;
        let bind: SocketAddr = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        }
        .parse()
        .map_err(failed)?;
        let endpoint = Endpoint::client(bind, self.identity.clone()).map_err(failed)?;
        let res = tokio::time::timeout(
            PAIR_WITH_TIMEOUT,
            pair_as_initiator(&endpoint, addr, &pin, &self.name),
        )
        .await;
        endpoint.close();
        let peer = match res {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::warn!(%addr, "emparejamiento fallido: {e}");
                return Err(failed(format!("No se pudo emparejar: {e}")));
            }
            Err(_) => {
                return Err(err(
                    ErrorCode::Timeout,
                    "El otro equipo no respondió en 30 s (¿dirección correcta? ¿firewall?)",
                ))
            }
        };
        self.add_trusted(peer.fingerprint, &peer.name).await?;
        tracing::info!(peer = %peer.fingerprint.short(), "emparejado (iniciador)");
        self.emit(Event::PeersChanged);
        let record = self
            .trust()
            .ok()
            .and_then(|t| t.get(&peer.fingerprint).cloned());
        Ok(Response::Paired {
            peer: match record {
                Some(r) => peer_info(&r),
                None => PeerInfo {
                    name: peer.name.clone(),
                    fingerprint: peer.fingerprint.to_string(),
                    fingerprint_short: peer.fingerprint.short(),
                    paired_at: 0,
                },
            },
        })
    }

    fn to_device(&self, p: DiscoveredPeer) -> DiscoveredDevice {
        let paired = self
            .trust()
            .map(|t| {
                t.fingerprints()
                    .iter()
                    .any(|fp| fp.device_id() == p.device_id)
            })
            .unwrap_or(false);
        DiscoveredDevice {
            fullname: p.fullname,
            device_id: p.device_id.to_string(),
            name: p.name,
            protocol_major: p.protocol_major,
            roles: p.roles.to_txt(),
            addresses: p.addresses.iter().map(|a| a.to_string()).collect(),
            session_port: p.session_port,
            pairing_port: p.pairing_port,
            paired,
        }
    }

    fn start_discovery(self: &Arc<Self>) -> Result<Response, IpcError> {
        let Some(m) = &self.mdns else {
            return Err(err(
                ErrorCode::Unsupported,
                "El servicio mDNS no pudo iniciarse en este equipo",
            ));
        };
        let mut st = self
            .discovery
            .lock()
            .map_err(|_| failed("estado de descubrimiento envenenado"))?;
        if !st.active {
            let own = Some(self.identity.fingerprint().device_id());
            let mut browser = m
                .lock()
                .map_err(|_| failed("estado mDNS envenenado"))?
                .browse(own)
                .map_err(|e| failed(format!("mDNS: {e}")))?;
            st.active = true;
            let agent = self.clone();
            tokio::spawn(async move {
                while let Some(ev) = browser.next().await {
                    match ev {
                        DiscoveryEvent::Found(p) => {
                            let dev = agent.to_device(p);
                            if let Ok(mut st) = agent.discovery.lock() {
                                st.devices.insert(dev.fullname.clone(), dev.clone());
                            }
                            agent.emit(Event::DeviceDiscovered(dev));
                        }
                        DiscoveryEvent::Lost { fullname } => {
                            if let Ok(mut st) = agent.discovery.lock() {
                                st.devices.remove(&fullname);
                            }
                            agent.emit(Event::DeviceLost { fullname });
                        }
                    }
                }
                tracing::warn!("la exploración mDNS terminó");
                if let Ok(mut st) = agent.discovery.lock() {
                    st.active = false;
                }
            });
            tracing::info!("exploración mDNS iniciada");
        }
        Ok(Response::Discovered {
            devices: st.devices.values().cloned().collect(),
        })
    }
}

impl Handler for Agent {
    fn handle(self: Arc<Self>, request: Request) -> HandlerFuture {
        Box::pin(self.dispatch(request))
    }
}

/// Monitores virtuales (`cs-display`). Las llamadas al SO son síncronas y se
/// ejecutan en un hilo bloqueante.
mod displays {
    use super::*;
    use cs_display::{DisplayError, Displays};

    fn map_err(e: DisplayError) -> IpcError {
        let code = match e {
            DisplayError::Unsupported => ErrorCode::Unsupported,
            DisplayError::NoVirtualTarget => ErrorCode::NotFound,
            DisplayError::ModeNotAvailable(_) => ErrorCode::BadRequest,
            _ => ErrorCode::Failed,
        };
        err(code, e.to_string())
    }

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
    ) -> Result<T, IpcError> {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| failed(format!("tarea interna: {e}")))?
    }

    fn mode_info(m: cs_core::Mode) -> ModeInfo {
        ModeInfo {
            width: m.width,
            height: m.height,
            refresh_mhz: m.refresh_mhz,
        }
    }

    pub async fn list() -> Result<Response, IpcError> {
        blocking(|| {
            let d = Displays::new();
            let mut out = Vec::new();
            for t in d.targets().map_err(map_err)? {
                let modes = match t.gdi_name.as_deref().filter(|_| t.is_virtual) {
                    Some(g) => d
                        .modes(g)
                        .map_err(map_err)?
                        .into_iter()
                        .map(mode_info)
                        .collect(),
                    None => Vec::new(),
                };
                out.push(DisplayInfo {
                    friendly_name: t.friendly_name,
                    device_path: t.device_path,
                    is_virtual: t.is_virtual,
                    active: t.active,
                    gdi_name: t.gdi_name,
                    modes,
                });
            }
            Ok(Response::Displays { displays: out })
        })
        .await
    }

    pub async fn show(width: u32, height: u32, hz: u32) -> Result<Response, IpcError> {
        blocking(move || {
            let mode = cs_core::Mode::new(width, height, hz);
            mode.validate()
                .map_err(|e| err(ErrorCode::BadRequest, e.to_string()))?;
            let s = Displays::new().show(None, mode).map_err(map_err)?;
            tracing::info!(gdi = %s.gdi_name, width, height, hz, "monitor virtual mostrado");
            Ok(Response::DisplayShown {
                gdi_name: s.gdi_name,
                mode: mode_info(s.mode),
            })
        })
        .await
    }

    pub async fn hide() -> Result<Response, IpcError> {
        blocking(|| {
            let d = Displays::new();
            let mut count = 0u32;
            for t in d
                .targets()
                .map_err(map_err)?
                .into_iter()
                .filter(|t| t.is_virtual && t.active)
            {
                d.hide(t.key).map_err(map_err)?;
                count += 1;
            }
            tracing::info!(count, "monitores virtuales retirados");
            Ok(Response::DisplaysHidden { count })
        })
        .await
    }
}
