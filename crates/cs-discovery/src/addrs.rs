//! Qué dirección de un par usar y cómo alcanzar una IPv6 de enlace local.
//!
//! mDNS devuelve todas las direcciones del otro equipo: también las de sus
//! interfaces virtuales (Docker, WSL, Hyper-V…), que desde aquí no llevan a
//! ninguna parte, y las IPv6 de enlace local (`fe80::/10`) **sin la interfaz**
//! por la que llegaron. Con un cable directo entre dos equipos no hay DHCP y
//! esas direcciones de enlace local son lo único que tienen en común; sin
//! interfaz (`%zona`) el sistema no sabe por dónde enviar (Linux rechaza el
//! envío y la conexión acaba en «timed out»).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Una dirección de una interfaz de red de este equipo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAddr {
    /// Nombre de la interfaz (`enp3s0`, `wlp4s0`, `Ethernet`, `Wi-Fi`…).
    pub iface: String,
    /// Índice de la interfaz: la zona (`scope_id`) de una IPv6 de enlace local.
    pub index: Option<u32>,
    pub ip: IpAddr,
    pub prefix_len: u8,
}

/// Direcciones de las interfaces de este equipo, sin las de loopback. Si no
/// se pueden leer, lista vacía: entonces no se reordena ni se prueban zonas.
pub fn local_addrs() -> Vec<LocalAddr> {
    match if_addrs::get_if_addrs() {
        Ok(list) => list
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| {
                let (ip, prefix_len) = match &i.addr {
                    if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
                    if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
                };
                LocalAddr {
                    iface: i.name,
                    index: i.index,
                    ip,
                    prefix_len,
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("no se pudieron leer las interfaces de red: {e}");
            Vec::new()
        }
    }
}

/// ¿Es una interfaz virtual cuya red no lleva a otro equipo físico? Docker,
/// puentes de contenedores y máquinas virtuales en Linux; en Windows, los
/// adaptadores `vEthernet` de WSL/Hyper-V, VirtualBox y VMware.
pub fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const PREFIXES: [&str; 13] = [
        "docker",
        "br-",
        "veth",
        "virbr",
        "vmnet",
        "vboxnet",
        "lxc",
        "lxdbr",
        "cni",
        "flannel",
        "podman",
        "vethernet",
        "vmware",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || n.contains("virtualbox")
}

/// IPv6 de enlace local (`fe80::/10`).
pub fn is_ipv6_link_local(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

fn same_subnet(a: IpAddr, b: IpAddr, prefix_len: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let bits = u32::from(prefix_len.min(32));
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let bits = u32::from(prefix_len.min(128));
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// Lo prometedora que es una dirección del par (menor es mejor).
fn rank(ip: IpAddr, locals: &[LocalAddr]) -> u8 {
    let physical = || locals.iter().filter(|l| !is_virtual_iface(&l.iface));
    let on_physical_subnet = || physical().any(|l| same_subnet(ip, l.ip, l.prefix_len));
    match ip {
        IpAddr::V4(v4) if v4.is_link_local() => {
            // 169.254.x.x (Windows sin DHCP): solo sirve si aquí también hay una.
            if on_physical_subnet() {
                1
            } else {
                5
            }
        }
        IpAddr::V4(_) => {
            if on_physical_subnet() {
                0
            } else if locals.iter().any(|l| same_subnet(ip, l.ip, l.prefix_len)) {
                // Misma red que una interfaz virtual de aquí (p. ej. los dos
                // equipos tienen Docker en 172.17.0.0/16): es la nuestra, no la suya.
                6
            } else {
                4
            }
        }
        IpAddr::V6(v6) if is_ipv6_link_local(&v6) => {
            if physical().any(|l| matches!(l.ip, IpAddr::V6(a) if is_ipv6_link_local(&a))) {
                2
            } else {
                5
            }
        }
        IpAddr::V6(_) => {
            if on_physical_subnet() {
                1
            } else {
                3
            }
        }
    }
}

/// Ordena las direcciones de un par de la más a la menos prometedora para
/// alcanzarlo desde este equipo (sin duplicados): primero las que están en
/// la misma red que una interfaz física de aquí, luego las IPv6 de enlace
/// local (cable directo) y al final las que parecen de redes virtuales.
pub fn rank_addresses(addrs: &[IpAddr], locals: &[LocalAddr]) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::with_capacity(addrs.len());
    for ip in addrs {
        if !out.contains(ip) {
            out.push(*ip);
        }
    }
    // Orden estable: a igual rango, IPv4 antes que IPv6 y luego por valor,
    // para que la elección no dependa del orden en que llegó cada registro.
    out.sort_by_key(|ip| (rank(*ip, locals), ip.is_ipv6(), *ip));
    out
}

/// Máximo de interfaces que se prueban a la vez para una IPv6 de enlace local.
const MAX_SCOPES: usize = 8;

/// Direcciones con las que intentar conectar con `addr`.
///
/// Si `addr` es una IPv6 de enlace local **sin zona**, una copia por cada
/// interfaz de este equipo que tenga IPv6 de enlace local (las físicas
/// primero; las virtuales solo si no hay físicas), para probarlas a la vez:
/// solo la del cable correcto llega al otro equipo. En cualquier otro caso,
/// `addr` tal cual.
pub fn connect_candidates(addr: SocketAddr, locals: &[LocalAddr]) -> Vec<SocketAddr> {
    let SocketAddr::V6(v6) = addr else {
        return vec![addr];
    };
    if v6.scope_id() != 0 || !is_ipv6_link_local(v6.ip()) {
        return vec![addr];
    }
    let with_ll = |virt: bool| -> Vec<u32> {
        let mut idx: Vec<u32> = Vec::new();
        for l in locals {
            let IpAddr::V6(ip) = l.ip else { continue };
            if !is_ipv6_link_local(&ip) || is_virtual_iface(&l.iface) != virt {
                continue;
            }
            if let Some(i) = l.index.filter(|i| *i != 0) {
                if !idx.contains(&i) {
                    idx.push(i);
                }
            }
        }
        idx
    };
    let mut scopes = with_ll(false);
    if scopes.is_empty() {
        scopes = with_ll(true);
    }
    if scopes.is_empty() {
        return vec![addr];
    }
    scopes
        .into_iter()
        .take(MAX_SCOPES)
        .map(|scope| {
            let mut s = v6;
            s.set_scope_id(scope);
            SocketAddr::V6(s)
        })
        .collect()
}

/// Dirección «cualquiera» de la misma familia que `addr`, para ligar un
/// cliente.
pub fn unspecified_for(addr: &SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(_) => SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
        SocketAddr::V6(_) => SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(iface: &str, index: u32, ip: &str, prefix_len: u8) -> LocalAddr {
        LocalAddr {
            iface: iface.into(),
            index: Some(index),
            ip: ip.parse().unwrap(),
            prefix_len,
        }
    }

    /// Como el portátil Linux del dueño: Wi-Fi con DHCP, cable directo sin
    /// IPv4, Docker y un puente de contenedores.
    fn linux_with_cable() -> Vec<LocalAddr> {
        vec![
            local("wlp4s0", 3, "192.168.1.139", 24),
            local("wlp4s0", 3, "fe80::46fe:e296:d0ad:9b32", 64),
            local("enp0s31f6", 2, "fe80::1:2:3:4", 64),
            local("docker0", 4, "172.17.0.1", 16),
            local("br-74379003f771", 5, "172.18.0.1", 16),
            local("br-74379003f771", 5, "fe80::1087:c4ff:feb9:1dc7", 64),
            local("veth5722204", 9, "fe80::e012:faff:fe98:fdb5", 64),
        ]
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn same_lan_ipv4_comes_first_and_virtual_networks_last() {
        let got = rank_addresses(
            &ips(&[
                "172.17.0.1",
                "fe80::290a:156e:4eb0:28e7",
                "172.25.80.1",
                "192.168.1.127",
            ]),
            &linux_with_cable(),
        );
        assert_eq!(
            got,
            ips(&[
                "192.168.1.127",
                "fe80::290a:156e:4eb0:28e7",
                "172.25.80.1",
                "172.17.0.1"
            ])
        );
    }

    #[test]
    fn direct_cable_prefers_link_local_over_unreachable_ipv4() {
        // Windows por cable directo: 169.254 (APIPA), fe80 y la red de WSL.
        let got = rank_addresses(
            &ips(&["172.25.80.1", "169.254.10.20", "fe80::290a:156e:4eb0:28e7"]),
            &linux_with_cable(),
        );
        assert_eq!(
            got[0],
            "fe80::290a:156e:4eb0:28e7".parse::<IpAddr>().unwrap()
        );
        // Con 169.254 también aquí, esa va primero.
        let mut locals = linux_with_cable();
        locals.push(local("enp0s31f6", 2, "169.254.99.1", 16));
        let got = rank_addresses(
            &ips(&["172.25.80.1", "fe80::290a:156e:4eb0:28e7", "169.254.10.20"]),
            &locals,
        );
        assert_eq!(got[0], "169.254.10.20".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn duplicates_are_removed() {
        let got = rank_addresses(&ips(&["192.168.1.2", "192.168.1.2"]), &[]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn unscoped_link_local_is_tried_on_each_physical_interface() {
        let addr: SocketAddr = "[fe80::290a:156e:4eb0:28e7]:54424".parse().unwrap();
        let got = connect_candidates(addr, &linux_with_cable());
        let scopes: Vec<u32> = got
            .iter()
            .map(|a| match a {
                SocketAddr::V6(v6) => v6.scope_id(),
                SocketAddr::V4(_) => panic!("se esperaba IPv6"),
            })
            .collect();
        // Wi-Fi (3) y cable (2); ni Docker, ni el puente, ni los veth.
        assert_eq!(scopes, vec![3, 2]);
        assert!(got.iter().all(|a| a.port() == 54424));
    }

    #[test]
    fn other_addresses_are_used_as_is() {
        let locals = linux_with_cable();
        for s in [
            "192.168.1.127:47810",
            "[fe80::1%7]:47810",
            "[fd00::5]:47810",
        ] {
            let addr: SocketAddr = s.parse().unwrap();
            assert_eq!(connect_candidates(addr, &locals), vec![addr]);
        }
        // Sin interfaces conocidas, la dirección tal cual.
        let ll: SocketAddr = "[fe80::1]:1".parse().unwrap();
        assert_eq!(connect_candidates(ll, &[]), vec![ll]);
    }

    #[test]
    fn virtual_interfaces_are_recognised() {
        for n in [
            "docker0",
            "br-74379003f771",
            "veth5722204",
            "virbr0",
            "vEthernet (WSL (Hyper-V firewall))",
            "VirtualBox Host-Only Network",
            "VMware Network Adapter VMnet8",
        ] {
            assert!(is_virtual_iface(n), "{n}");
        }
        for n in ["enp0s31f6", "eth0", "wlp4s0", "Ethernet", "Wi-Fi"] {
            assert!(!is_virtual_iface(n), "{n}");
        }
    }

    #[test]
    fn subnet_math() {
        let a: IpAddr = "192.168.1.10".parse().unwrap();
        assert!(same_subnet(a, "192.168.1.200".parse().unwrap(), 24));
        assert!(!same_subnet(a, "192.168.2.1".parse().unwrap(), 24));
        assert!(same_subnet(a, "10.0.0.1".parse().unwrap(), 0));
        let b: IpAddr = "fd00::1".parse().unwrap();
        assert!(same_subnet(b, "fd00::ffff".parse().unwrap(), 64));
        assert!(!same_subnet(b, "fd01::1".parse().unwrap(), 64));
        assert!(!same_subnet(a, b, 24));
    }
}
