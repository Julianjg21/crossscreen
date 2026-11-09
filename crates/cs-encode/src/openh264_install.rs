//! Descarga e instalación del binario oficial de OpenH264 de Cisco (Linux).
//!
//! CrossScreen no puede redistribuir OpenH264: la licencia de patentes de
//! H.264 la cubre Cisco solo si el usuario descarga **su** binario. Esto hace
//! lo mismo que `scripts/fetch-openh264.sh`, sin depender de `curl` ni
//! `bzip2`: descarga de `ciscobinary.openh264.org`, descomprime, verifica el
//! SHA-256 fijado (la integridad no depende del transporte, que Cisco sirve
//! por HTTP) e instala en `~/.local/share/crossscreen/`.

use crate::openh264_backend::OPENH264_BLOB;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

/// URL oficial de Cisco del binario comprimido.
pub const OPENH264_URL: &str = "http://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2";
/// SHA-256 del binario descomprimido (el mismo que `fetch-openh264.sh`).
pub const OPENH264_SHA256: &str =
    "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04";
/// Tope del archivo comprimido (el real ronda 0,6 MiB).
const MAX_COMPRESSED: u64 = 16 * 1024 * 1024;
/// Tope del binario descomprimido (el real ronda 1,3 MiB).
const MAX_DECOMPRESSED: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("OpenH264 de Cisco solo está disponible para Linux x86_64 en CrossScreen")]
    UnsupportedArch,
    #[error("no se pudo determinar la carpeta personal (HOME)")]
    NoHome,
    #[error("no se pudo descargar OpenH264 de Cisco: {0}")]
    Download(String),
    #[error("el archivo descargado no es válido: {0}")]
    Corrupt(String),
    #[error("el SHA-256 no coincide (esperado {OPENH264_SHA256}, obtenido {0}): no se instala")]
    BadHash(String),
    #[error("no se pudo guardar OpenH264 en {path}: {err}")]
    Io { path: String, err: std::io::Error },
}

/// Carpeta donde se instala (`~/.local/share/crossscreen`), la misma en la
/// que lo buscan el emisor y el visor.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/crossscreen"))
}

/// Ruta del binario si ya está disponible donde lo buscan los programas.
pub fn installed_path() -> Option<PathBuf> {
    crate::openh264_backend::locate_blob().ok()
}

/// Descarga, verifica e instala OpenH264. Bloqueante (red y disco): en un
/// runtime async, llamarla con `spawn_blocking`.
pub fn install() -> Result<PathBuf, InstallError> {
    if !cfg!(target_arch = "x86_64") {
        return Err(InstallError::UnsupportedArch);
    }
    let dir = install_dir().ok_or(InstallError::NoHome)?;
    let compressed = download(OPENH264_URL)?;
    let blob = decompress(&compressed)?;
    verify(&blob)?;
    let io = |path: &PathBuf, err| InstallError::Io {
        path: path.display().to_string(),
        err,
    };
    std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
    let target = dir.join(OPENH264_BLOB);
    // Escritura atómica: nunca queda un binario a medias donde se busca.
    let tmp = dir.join(format!("{OPENH264_BLOB}.part"));
    std::fs::write(&tmp, &blob).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, &target).map_err(|e| io(&target, e))?;
    tracing::info!("OpenH264 instalado y verificado en {}", target.display());
    Ok(target)
}

fn download(url: &str) -> Result<Vec<u8>, InstallError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| InstallError::Download(e.to_string()))?;
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_COMPRESSED + 1)
        .read_to_end(&mut body)
        .map_err(|e| InstallError::Download(e.to_string()))?;
    if body.len() as u64 > MAX_COMPRESSED {
        return Err(InstallError::Corrupt("descarga demasiado grande".into()));
    }
    Ok(body)
}

fn decompress(compressed: &[u8]) -> Result<Vec<u8>, InstallError> {
    let mut out = Vec::new();
    bzip2_rs::DecoderReader::new(compressed)
        .take(MAX_DECOMPRESSED + 1)
        .read_to_end(&mut out)
        .map_err(|e| InstallError::Corrupt(format!("bzip2: {e}")))?;
    if out.len() as u64 > MAX_DECOMPRESSED {
        return Err(InstallError::Corrupt(
            "descomprimido demasiado grande".into(),
        ));
    }
    Ok(out)
}

fn verify(blob: &[u8]) -> Result<(), InstallError> {
    let got = hex(&Sha256::digest(blob));
    if got == OPENH264_SHA256 {
        Ok(())
    } else {
        Err(InstallError::BadHash(got))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{decompress, verify, InstallError};

    #[test]
    fn rejects_wrong_hash_and_garbage() {
        assert!(matches!(
            verify(b"no es openh264"),
            Err(InstallError::BadHash(_))
        ));
        assert!(matches!(
            decompress(b"esto no es bzip2"),
            Err(InstallError::Corrupt(_))
        ));
    }
}
