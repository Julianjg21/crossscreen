//! Estado y lógica del agente. Todo lo que se expone por IPC es funcionalidad
//! real del núcleo: no hay datos simulados.

use anyhow::{Context, Result};
use cs_core::Roles;
use cs_discovery::{Announcement, DiscoveredPeer, Discovery, DiscoveryEvent};
use cs_ipc::{
    AgentStatus, DiscoveredDevice, DisplayInfo, ErrorCode, Event, Handler, HandlerFuture,
    IdentityInfo, IpcError, LatencySummary, ModeInfo, PairingOutcome, PeerInfo, Request, Response,
    StreamModeInfo, StreamReportInfo, StreamSessionInfo, IPC_VERSION,
};
use cs_protocol::{ALPN, ALPN_PAIRING, PROTOCOL_VERSION};
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
/// Tiempo máximo para conectar por QUIC al receptor de streaming.
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

struct PairingListener {
    pin: String,
    port: u16,
    cancel: Option<oneshot::Sender<()>>,
}

struct ActiveStream {
    session: StreamSessionInfo,
    stop: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct StreamState {
    active: Option<ActiveStream>,
    last: Option<Box<StreamReportInfo>>,
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
    stream: tokio::sync::Mutex<StreamState>,
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
            stream: tokio::sync::Mutex::new(StreamState::default()),
            discovery: Mutex::new(DiscoveryState::default()),
        });
        agent.announce(None);
        Ok(agent)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn shutdown(&self) {
        if let Some(m) = &self.mdns {
            if let Ok(mut d) = m.lock() {
                let _ = d.withdraw();
            }
        }
        let stop = {
            let st = self.stream.lock().await;
            st.active.as_ref().map(|a| a.stop.clone())
        };
        if let Some(stop) = stop {
            let _ = stop.send(true);
            for _ in 0..30 {
                if self.stream.lock().await.active.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
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
            Request::StartStream {
                addr,
                peer,
                mode,
                mode_override,
                bitrate_kbps,
            } => {
                self.start_stream(&addr, &peer, mode, mode_override, bitrate_kbps)
                    .await
            }
            Request::StopStream => self.stop_stream().await,
        }
    }

    async fn status(&self) -> AgentStatus {
        let pairing_port = self.pairing.lock().await.as_ref().map(|p| p.port);
        let discovery_active = self.discovery.lock().map(|d| d.active).unwrap_or(false);
        let (active_stream, last_stream) = {
            let st = self.stream.lock().await;
            (
                st.active.as_ref().map(|a| a.session.clone()),
                st.last.clone(),
            )
        };
        AgentStatus {
            agent_version: env!("CARGO_PKG_VERSION").into(),
            ipc_version: IPC_VERSION,
            platform: std::env::consts::OS.into(),
            pairing_port,
            discovery_active,
            mdns_available: self.mdns.is_some(),
            displays_supported: cfg!(windows),
            streaming_supported: cfg!(windows),
            active_stream,
            last_stream,
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

    async fn start_stream(
        self: &Arc<Self>,
        addr: &str,
        peer: &str,
        mode: StreamModeInfo,
        mode_override: Option<ModeInfo>,
        bitrate_kbps: u32,
    ) -> Result<Response, IpcError> {
        if !cfg!(windows) {
            return Err(err(
                ErrorCode::Unsupported,
                "El emisor de pantalla solo está disponible en Windows por ahora",
            ));
        }
        let addr: SocketAddr = addr.trim().parse().map_err(|_| {
            err(
                ErrorCode::BadRequest,
                "Dirección no válida (usa IP:puerto, p. ej. 192.168.1.20:47810)",
            )
        })?;
        if !(500..=500_000).contains(&bitrate_kbps) {
            return Err(err(
                ErrorCode::BadRequest,
                "La tasa de bits debe estar entre 500 y 500 000 kbps",
            ));
        }
        let requested_mode = match mode_override {
            Some(m) => {
                let cm = cs_core::Mode {
                    width: m.width,
                    height: m.height,
                    refresh_mhz: m.refresh_mhz,
                };
                cm.validate()
                    .map_err(|e| err(ErrorCode::BadRequest, e.to_string()))?;
                Some(cm)
            }
            None => None,
        };
        let wanted = peer.trim().replace('-', "").to_lowercase();
        if wanted.is_empty() {
            return Err(err(
                ErrorCode::BadRequest,
                "Selecciona un equipo receptor emparejado",
            ));
        }
        let peer_record = {
            let _g = self.trust_lock.lock().await;
            let trust = self.trust()?;
            let matches: Vec<PeerRecord> = trust
                .peers()
                .filter(|p| p.fingerprint.to_string().starts_with(&wanted))
                .cloned()
                .collect();
            match matches.as_slice() {
                [one] => one.clone(),
                [] => {
                    return Err(err(
                        ErrorCode::NotFound,
                        "Ese equipo no está en la lista de emparejados",
                    ))
                }
                _ => {
                    return Err(err(
                        ErrorCode::BadRequest,
                        "El prefijo de huella coincide con varios equipos",
                    ))
                }
            }
        };
        if self.stream.lock().await.active.is_some() {
            return Err(err(
                ErrorCode::Busy,
                "Ya hay una transmisión en curso. Deténla antes de iniciar otra.",
            ));
        }
        let bind: SocketAddr = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        }
        .parse()
        .expect("literal válido");
        let endpoint = Endpoint::client(bind, self.identity.clone()).map_err(failed)?;
        let conn = match tokio::time::timeout(
            STREAM_CONNECT_TIMEOUT,
            endpoint.connect(addr, PeerPolicy::Exactly(peer_record.fingerprint), ALPN),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                endpoint.close();
                return Err(failed(format!("No se pudo conectar con {addr}: {e}")));
            }
            Err(_) => {
                endpoint.close();
                return Err(err(
                    ErrorCode::Timeout,
                    format!(
                        "Tiempo agotado al conectar con {addr}. Comprueba que el visor (crossscreen-viewer --listen) esté activo en ese puerto."
                    ),
                ));
            }
        };
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let session = StreamSessionInfo {
            peer_name: peer_record.name.clone(),
            peer_fingerprint: peer_record.fingerprint.to_string(),
            peer_fingerprint_short: peer_record.fingerprint.short(),
            remote_addr: conn.remote_addr().to_string(),
            mode,
            mode_override,
            bitrate_kbps,
            initial_rtt_us: conn.rtt().as_micros() as u64,
            started_at,
        };
        let mut cfg = cs_pipeline::SenderConfig::new(
            self.identity.fingerprint().device_id(),
            self.name.clone(),
        );
        cfg.mode = match mode {
            StreamModeInfo::Extend => cs_core::StreamMode::Extend,
            StreamModeInfo::Duplicate => cs_core::StreamMode::Duplicate,
        };
        cfg.requested_mode = requested_mode;
        cfg.bitrate_kbps = bitrate_kbps;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        {
            let mut st = self.stream.lock().await;
            if st.active.is_some() {
                conn.close(b"busy");
                endpoint.close();
                return Err(err(ErrorCode::Busy, "Ya hay una transmisión en curso"));
            }
            st.active = Some(ActiveStream {
                session: session.clone(),
                stop: stop_tx,
            });
        }
        tracing::info!(
            peer = %session.peer_fingerprint_short,
            addr = %session.remote_addr,
            mode = ?session.mode,
            "transmisión iniciada"
        );
        self.emit(Event::StreamStarted {
            session: session.clone(),
        });
        let agent = self.clone();
        let sess_for_task = session.clone();
        tokio::spawn(async move {
            let res = cs_pipeline::run_sender(conn, cfg, stop_rx).await;
            endpoint.close();
            let report = Box::new(build_stream_report(&sess_for_task, res));
            if let Some(e) = &report.error {
                tracing::warn!(
                    peer = %sess_for_task.peer_fingerprint_short,
                    error = %e,
                    "transmisión terminada con error"
                );
            } else {
                tracing::info!(
                    peer = %sess_for_task.peer_fingerprint_short,
                    reason = %report.end_reason,
                    frames = report.frames_sent,
                    "transmisión terminada"
                );
            }
            {
                let mut st = agent.stream.lock().await;
                st.active = None;
                st.last = Some(report.clone());
            }
            agent.emit(Event::StreamEnded { report });
        });
        Ok(Response::StreamStarted { session })
    }

    async fn stop_stream(&self) -> Result<Response, IpcError> {
        let stop = {
            let st = self.stream.lock().await;
            st.active.as_ref().map(|a| a.stop.clone())
        };
        let Some(stop) = stop else {
            return Err(err(
                ErrorCode::NotFound,
                "No hay ninguna transmisión en curso",
            ));
        };
        let _ = stop.send(true);
        Ok(Response::Ok)
    }
}

fn build_stream_report(
    session: &StreamSessionInfo,
    res: anyhow::Result<cs_pipeline::SenderReport>,
) -> StreamReportInfo {
    match res {
        Ok(mut r) => {
            let w = &mut r.worker;
            let s = &mut r.send;
            let c = &r.control;
            let rs = c.last_receiver_stats.as_ref();
            let capture_us = match (
                w.capture_us.percentile(50.0),
                w.capture_us.percentile(95.0),
                w.capture_us.max(),
            ) {
                (Some(p50), Some(p95), Some(max)) => Some(LatencySummary {
                    p50_us: p50.round() as u64,
                    p95_us: p95.round() as u64,
                    max_us: max.round() as u64,
                    samples: w.capture_us.len() as u64,
                }),
                _ => None,
            };
            let encode_us = match (
                w.encode_us.percentile(50.0),
                w.encode_us.percentile(95.0),
                w.encode_us.max(),
            ) {
                (Some(p50), Some(p95), Some(max)) => Some(LatencySummary {
                    p50_us: p50.round() as u64,
                    p95_us: p95.round() as u64,
                    max_us: max.round() as u64,
                    samples: w.encode_us.len() as u64,
                }),
                _ => None,
            };
            let capture_to_sent_us = match (
                s.capture_to_sent_us.percentile(50.0),
                s.capture_to_sent_us.percentile(95.0),
                s.capture_to_sent_us.max(),
            ) {
                (Some(p50), Some(p95), Some(max)) => Some(LatencySummary {
                    p50_us: p50.round() as u64,
                    p95_us: p95.round() as u64,
                    max_us: max.round() as u64,
                    samples: s.capture_to_sent_us.len() as u64,
                }),
                _ => None,
            };
            StreamReportInfo {
                peer_name: session.peer_name.clone(),
                remote_addr: session.remote_addr.clone(),
                duration_ms: r.duration.as_millis() as u64,
                end_reason: r.end_reason,
                captured_size: w.last_size,
                frames_captured: w.captured,
                frames_encoded: w.encoded,
                frames_sent: s.frames_sent,
                dropped_before_encode: w.dropped_backpressure,
                dropped_after_encode: w.dropped_after_encode,
                idr_sent: s.keyframes_sent,
                keyframe_requests: c.keyframe_requests,
                payload_bytes: s.payload_bytes,
                wire_bytes: s.wire_bytes,
                datagrams_sent: s.datagrams_sent,
                datagrams_failed: s.datagrams_failed,
                cursor_positions: c.cursor_positions,
                cursor_shapes: c.cursor_shapes,
                capture_us,
                encode_us,
                capture_to_sent_us,
                receiver_frames_received: rs.map(|x| u64::from(x.frames_received)),
                receiver_frames_lost: rs.map(|x| u64::from(x.frames_lost)),
                receiver_jitter_us: rs.map(|x| x.jitter_us),
                receiver_decode_us: rs.map(|x| x.decode_time_us),
                error: w.fatal.clone(),
            }
        }
        Err(e) => StreamReportInfo {
            peer_name: session.peer_name.clone(),
            remote_addr: session.remote_addr.clone(),
            duration_ms: 0,
            end_reason: "error al iniciar la sesión".into(),
            captured_size: None,
            frames_captured: 0,
            frames_encoded: 0,
            frames_sent: 0,
            dropped_before_encode: 0,
            dropped_after_encode: 0,
            idr_sent: 0,
            keyframe_requests: 0,
            payload_bytes: 0,
            wire_bytes: 0,
            datagrams_sent: 0,
            datagrams_failed: 0,
            cursor_positions: 0,
            cursor_shapes: 0,
            capture_us: None,
            encode_us: None,
            capture_to_sent_us: None,
            receiver_frames_received: None,
            receiver_frames_lost: None,
            receiver_jitter_us: None,
            receiver_decode_us: None,
            error: Some(format!("{e:#}")),
        },
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
