//! Descubrimiento por mDNS/DNS-SD (servicio `_crossscreen._udp.local.`).
//!
//! Los registros TXT **no contienen secretos**: solo el id público, la
//! versión mayor del protocolo, los roles y, mientras el usuario tiene abierto
//! el diálogo de emparejamiento, el puerto del listener de emparejamiento.
//! La autenticación se hace siempre con huellas fijadas (ver `cs-security`).

use cs_core::{DeviceId, Roles};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::IpAddr;

pub const SERVICE_TYPE: &str = "_crossscreen._udp.local.";

const TXT_ID: &str = "id";
const TXT_VERSION: &str = "v";
const TXT_ROLES: &str = "r";
const TXT_NAME: &str = "n";
const TXT_PAIRING_PORT: &str = "pp";

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mDNS: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// Lo que este equipo anuncia.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub device_id: DeviceId,
    pub name: String,
    pub roles: Roles,
    pub protocol_major: u16,
    pub session_port: u16,
    pub pairing_port: Option<u16>,
}

/// Un par descubierto en la red.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub fullname: String,
    pub device_id: DeviceId,
    pub name: String,
    pub roles: Roles,
    pub protocol_major: u16,
    pub addresses: Vec<IpAddr>,
    pub session_port: u16,
    pub pairing_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Found(DiscoveredPeer),
    Lost { fullname: String },
}

impl Announcement {
    fn txt(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(TXT_ID.into(), self.device_id.to_string());
        m.insert(TXT_VERSION.into(), self.protocol_major.to_string());
        m.insert(TXT_ROLES.into(), self.roles.to_txt());
        m.insert(TXT_NAME.into(), self.name.chars().take(63).collect());
        if let Some(p) = self.pairing_port {
            m.insert(TXT_PAIRING_PORT.into(), p.to_string());
        }
        m
    }
}

/// Interpreta los campos TXT de un servicio resuelto. Devuelve `None` si el
/// anuncio no es de CrossScreen o está mal formado (se ignora, no es error).
pub fn parse_peer(
    fullname: &str,
    txt: &HashMap<String, String>,
    addresses: Vec<IpAddr>,
    port: u16,
) -> Option<DiscoveredPeer> {
    let device_id = txt.get(TXT_ID)?.parse().ok()?;
    let protocol_major = txt.get(TXT_VERSION)?.parse().ok()?;
    let roles = Roles::from_txt(txt.get(TXT_ROLES).map(String::as_str).unwrap_or(""));
    let name = txt
        .get(TXT_NAME)
        .map(|n| {
            n.chars()
                .filter(|c| !c.is_control())
                .take(63)
                .collect::<String>()
        })
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| fullname.split('.').next().unwrap_or("?").to_owned());
    let pairing_port = txt.get(TXT_PAIRING_PORT).and_then(|p| p.parse().ok());
    Some(DiscoveredPeer {
        fullname: fullname.to_owned(),
        device_id,
        name,
        roles,
        protocol_major,
        addresses,
        session_port: port,
        pairing_port,
    })
}

/// Servicio mDNS: anuncia este equipo y explora la red.
pub struct Discovery {
    daemon: ServiceDaemon,
    registered: Option<String>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery")
            .field("registered", &self.registered)
            .finish_non_exhaustive()
    }
}

impl Discovery {
    pub fn new() -> Result<Self, DiscoveryError> {
        Ok(Discovery {
            daemon: ServiceDaemon::new()?,
            registered: None,
        })
    }

    /// Registra (o reemplaza) el anuncio de este equipo.
    pub fn announce(&mut self, a: &Announcement) -> Result<(), DiscoveryError> {
        self.withdraw()?;
        // Nombre de instancia único por dispositivo; el nombre visible va en TXT.
        let instance = format!("cs-{}", a.device_id);
        let host = format!("{instance}.local.");
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", a.session_port, a.txt())?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info)?;
        self.registered = Some(fullname);
        Ok(())
    }

    pub fn withdraw(&mut self) -> Result<(), DiscoveryError> {
        if let Some(name) = self.registered.take() {
            let _ = self.daemon.unregister(&name)?;
        }
        Ok(())
    }

    /// Empieza a explorar. Los eventos de nuestro propio anuncio se filtran
    /// pasando `own_id`.
    pub fn browse(&self, own_id: Option<DeviceId>) -> Result<Browser, DiscoveryError> {
        Ok(Browser {
            rx: self.daemon.browse(SERVICE_TYPE)?,
            own_id,
        })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.withdraw();
        let _ = self.daemon.shutdown();
    }
}

pub struct Browser {
    rx: mdns_sd::Receiver<ServiceEvent>,
    own_id: Option<DeviceId>,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}

impl Browser {
    /// Siguiente evento relevante; `None` si el demonio mDNS se detuvo.
    pub async fn next(&mut self) -> Option<DiscoveryEvent> {
        loop {
            let ev = self.rx.recv_async().await.ok()?;
            match ev {
                ServiceEvent::ServiceResolved(info) => {
                    let txt: HashMap<String, String> = info
                        .get_properties()
                        .iter()
                        .map(|p| (p.key().to_owned(), p.val_str().to_owned()))
                        .collect();
                    let addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                    let Some(peer) = parse_peer(info.get_fullname(), &txt, addrs, info.get_port())
                    else {
                        tracing::debug!(
                            name = info.get_fullname(),
                            "anuncio mDNS ignorado (TXT inválido)"
                        );
                        continue;
                    };
                    if Some(peer.device_id) == self.own_id {
                        continue;
                    }
                    return Some(DiscoveryEvent::Found(peer));
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    return Some(DiscoveryEvent::Lost { fullname })
                }
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann() -> Announcement {
        Announcement {
            device_id: DeviceId([0xAB; 16]),
            name: "Portátil de Ana".into(),
            roles: Roles::BOTH,
            protocol_major: 0,
            session_port: 47000,
            pairing_port: Some(47001),
        }
    }

    #[test]
    fn txt_roundtrip() {
        let a = ann();
        let p = parse_peer(
            "cs-x._crossscreen._udp.local.",
            &a.txt(),
            vec![],
            a.session_port,
        )
        .unwrap();
        assert_eq!(p.device_id, a.device_id);
        assert_eq!(p.name, a.name);
        assert_eq!(p.roles, a.roles);
        assert_eq!(p.pairing_port, Some(47001));
        assert_eq!(p.session_port, 47000);
    }

    #[test]
    fn malformed_txt_is_ignored() {
        let mut t = ann().txt();
        t.insert("id".into(), "no-hex".into());
        assert!(parse_peer("x", &t, vec![], 1).is_none());
        let mut t = ann().txt();
        t.remove("v");
        assert!(parse_peer("x", &t, vec![], 1).is_none());
    }

    #[test]
    fn control_chars_in_name_are_stripped() {
        let mut t = ann().txt();
        t.insert("n".into(), "evil\u{1b}[2Jname".into());
        let p = parse_peer("cs-x._crossscreen._udp.local.", &t, vec![], 1).unwrap();
        assert!(!p.name.contains('\u{1b}'));
    }
}
