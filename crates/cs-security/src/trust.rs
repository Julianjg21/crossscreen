//! Almacén persistente de dispositivos emparejados.

use crate::{Fingerprint, SecurityError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// Segundos desde la época Unix.
    pub paired_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    version: u32,
    peers: BTreeMap<String, PeerRecord>,
}

#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    file: TrustFile,
}

impl TrustStore {
    const FILE_VERSION: u32 = 1;

    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SecurityError> {
        let path = path.into();
        let file = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustFile {
                version: Self::FILE_VERSION,
                peers: BTreeMap::new(),
            },
            Err(e) => return Err(e.into()),
        };
        Ok(TrustStore { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_trusted(&self, fp: &Fingerprint) -> bool {
        self.file.peers.contains_key(&fp.to_string())
    }

    pub fn get(&self, fp: &Fingerprint) -> Option<&PeerRecord> {
        self.file.peers.get(&fp.to_string())
    }

    pub fn peers(&self) -> impl Iterator<Item = &PeerRecord> {
        self.file.peers.values()
    }

    pub fn fingerprints(&self) -> Vec<Fingerprint> {
        self.file.peers.values().map(|p| p.fingerprint).collect()
    }

    pub fn add(
        &mut self,
        fingerprint: Fingerprint,
        name: impl Into<String>,
    ) -> Result<(), SecurityError> {
        let paired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.file.peers.insert(
            fingerprint.to_string(),
            PeerRecord {
                fingerprint,
                name: name.into(),
                paired_at,
            },
        );
        self.save()
    }

    pub fn remove(&mut self, fp: &Fingerprint) -> Result<bool, SecurityError> {
        let removed = self.file.peers.remove(&fp.to_string()).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Escritura atómica: archivo temporal + renombrado.
    fn save(&self) -> Result<(), SecurityError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.file)?)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_persist_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let fp = Fingerprint([5; 32]);
        {
            let mut s = TrustStore::open(&path).unwrap();
            assert!(!s.is_trusted(&fp));
            s.add(fp, "portátil").unwrap();
        }
        let mut s = TrustStore::open(&path).unwrap();
        assert!(s.is_trusted(&fp));
        assert_eq!(s.get(&fp).unwrap().name, "portátil");
        assert!(s.remove(&fp).unwrap());
        assert!(!TrustStore::open(&path).unwrap().is_trusted(&fp));
    }

    #[test]
    fn corrupt_file_is_an_error_not_empty_trust() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        fs::write(&path, b"{not json").unwrap();
        assert!(TrustStore::open(&path).is_err());
    }
}
