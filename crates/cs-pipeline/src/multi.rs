//! Varios monitores por sesión (ADR-0009).
//!
//! Cada monitor es una sesión emisora **independiente**: su monitor virtual,
//! su conexión QUIC y, en el receptor, su propio visor. El monitor `i` usa el
//! puerto base del receptor más `i`. Así se reutiliza entero el camino de un
//! monitor (reconexión, FEC, cursor, entrada) sin tocar el protocolo.
//!
//! Lo que es único por equipo (portapapeles, audio del sistema y archivos)
//! viaja solo con el primer monitor; la entrada remota va con cada uno (cada visor
//! controla su propio monitor).

use crate::SenderConfig;
use std::net::SocketAddr;

/// Máximo de monitores por sesión.
pub const MAX_MONITORS: u8 = 4;

/// Dirección del receptor del monitor `index` (puerto base + `index`).
/// `None` si el puerto se sale del rango.
pub fn monitor_addr(base: SocketAddr, index: u8) -> Option<SocketAddr> {
    let port = base.port().checked_add(u16::from(index))?;
    let mut a = base;
    a.set_port(port);
    Some(a)
}

/// Configuración del monitor `index` a partir de la del primero.
pub fn monitor_config(base: &SenderConfig, index: u8) -> SenderConfig {
    let mut cfg = base.clone();
    if index > 0 {
        cfg.clipboard = false;
        cfg.audio = false;
        cfg.files = None;
    }
    if let Some(r) = cfg.reconnect.as_mut() {
        if let Some(a) = monitor_addr(r.addr, index) {
            r.addr = a;
        }
    }
    cfg
}

/// Valida el número de monitores pedido (0 cuenta como 1).
pub fn check_monitors(n: u8, mode: cs_core::StreamMode) -> Result<u8, String> {
    let n = n.max(1);
    if n > MAX_MONITORS {
        return Err(format!("Como máximo {MAX_MONITORS} monitores por sesión"));
    }
    if n > 1 && mode != cs_core::StreamMode::Extend {
        return Err("Varios monitores solo en modo «extender»".into());
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_core::{DeviceId, StreamMode};

    #[test]
    fn ports_follow_the_base() {
        let base: SocketAddr = "192.168.1.20:47810".parse().unwrap();
        assert_eq!(monitor_addr(base, 0), Some(base));
        assert_eq!(
            monitor_addr(base, 2),
            Some("192.168.1.20:47812".parse().unwrap())
        );
        let top: SocketAddr = "10.0.0.1:65535".parse().unwrap();
        assert_eq!(monitor_addr(top, 1), None);
    }

    #[test]
    fn clipboard_and_audio_only_on_the_first_monitor() {
        let mut cfg = SenderConfig::new(DeviceId::from_fingerprint(&[7; 32]), "pc".into());
        cfg.clipboard = true;
        cfg.audio = true;
        cfg.accept_input = true;
        let first = monitor_config(&cfg, 0);
        assert!(first.clipboard && first.audio && first.accept_input);
        let second = monitor_config(&cfg, 1);
        assert!(!second.clipboard && !second.audio);
        assert!(second.accept_input, "cada visor controla su monitor");
    }

    #[test]
    fn monitors_are_validated() {
        assert_eq!(check_monitors(0, StreamMode::Duplicate), Ok(1));
        assert_eq!(check_monitors(1, StreamMode::Duplicate), Ok(1));
        assert_eq!(check_monitors(3, StreamMode::Extend), Ok(3));
        assert!(check_monitors(2, StreamMode::Duplicate).is_err());
        assert!(check_monitors(MAX_MONITORS + 1, StreamMode::Extend).is_err());
    }
}
