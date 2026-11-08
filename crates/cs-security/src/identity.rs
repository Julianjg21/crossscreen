//! Identidad criptográfica del dispositivo.
//!
//! El certificado (público) se guarda como `device-cert.der` en el directorio
//! de datos. La clave privada PKCS#8 se guarda así:
//!
//! - **Windows:** cifrada con DPAPI (`CryptProtectData`, ámbito del usuario
//!   actual, con entropía adicional fija) en `device-key.dpapi`. Solo la misma
//!   cuenta de Windows puede descifrarla. Si existe un `device-key.pk8` en
//!   claro de versiones anteriores, se migra automáticamente: se cifra, se
//!   comprueba que el blob se descifra bien y el archivo en claro se
//!   sobrescribe con ceros y se borra (borrado *best effort*: en SSD/NTFS no
//!   hay garantía de que no queden copias físicas).
//! - **Linux y demás Unix:** archivo PKCS#8 DER `device-key.pk8` con permisos
//!   `0600`. La integración con Secret Service prevista en el ADR-0004 está
//!   pendiente y no cambiará esta API.
//!
//! Si la clave existe pero no se puede descifrar o leer, se devuelve un error
//! en lugar de regenerar la identidad: regenerarla cambiaría la huella y
//! rompería todos los emparejamientos sin avisar.

use crate::{Fingerprint, SecurityError};
use rcgen::{CertificateParams, DnType, KeyPair, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs;
use std::path::Path;

const CERT_FILE: &str = "device-cert.der";
/// Clave en claro (Unix; en Windows solo como origen de la migración).
const KEY_FILE: &str = "device-key.pk8";
/// Clave cifrada con DPAPI (Windows).
#[cfg(windows)]
const KEY_FILE_DPAPI: &str = "device-key.dpapi";
#[cfg(windows)]
const DPAPI_ENTROPY: &[u8] = b"CrossScreen device key v1";
#[cfg(windows)]
const DPAPI_DESCRIPTION: &str = "CrossScreen device key";
/// Nombre DNS fijo en el certificado. No se usa para autenticar (se fijan
/// huellas), pero rustls exige un `ServerName`.
pub const CERT_SERVER_NAME: &str = "crossscreen.invalid";

#[derive(Clone)]
pub struct Identity {
    cert_der: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    fingerprint: Fingerprint,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nunca imprimir la clave privada.
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> Result<Self, SecurityError> {
        let key = KeyPair::generate_for(&PKCS_ED25519)?;
        let mut params = CertificateParams::new(vec![CERT_SERVER_NAME.to_string()])?;
        params
            .distinguished_name
            .push(DnType::CommonName, "CrossScreen device");
        let cert = params.self_signed(&key)?;
        Ok(Self::from_parts(cert.der().clone(), key.serialize_der()))
    }

    fn from_parts(cert_der: CertificateDer<'static>, key_pkcs8: Vec<u8>) -> Self {
        let fingerprint = Fingerprint::of_cert_der(&cert_der);
        Identity {
            cert_der,
            key_pkcs8,
            fingerprint,
        }
    }

    /// Carga la identidad del directorio o la crea si no existe.
    pub fn load_or_create(dir: &Path) -> Result<Self, SecurityError> {
        let cert_path = dir.join(CERT_FILE);
        if cert_path.exists() {
            if let Some(key) = key_store::load(dir)? {
                // Comprobar que la clave es legible.
                KeyPair::try_from(key.as_slice())
                    .map_err(|_| SecurityError::InvalidIdentity("clave privada ilegible"))?;
                let cert = fs::read(&cert_path)?;
                return Ok(Self::from_parts(CertificateDer::from(cert), key));
            }
        }
        let id = Self::generate()?;
        fs::create_dir_all(dir)?;
        fs::write(&cert_path, id.cert_der.as_ref())?;
        key_store::store(dir, &id.key_pkcs8)?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

/// Persistencia de la clave privada con DPAPI (Windows).
#[cfg(windows)]
mod key_store {
    use super::*;
    use crate::dpapi;
    use std::io::{self, Write};

    /// Devuelve la clave en claro, o `None` si no hay clave guardada.
    pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, SecurityError> {
        let enc_path = dir.join(KEY_FILE_DPAPI);
        let plain_path = dir.join(KEY_FILE);
        if enc_path.exists() {
            let key = dpapi::unprotect(&fs::read(&enc_path)?, DPAPI_ENTROPY)?;
            if plain_path.exists() {
                // Restos de una migración interrumpida: manda la versión cifrada.
                secure_remove(&plain_path)?;
            }
            return Ok(Some(key));
        }
        if plain_path.exists() {
            let key = fs::read(&plain_path)?;
            KeyPair::try_from(key.as_slice())
                .map_err(|_| SecurityError::InvalidIdentity("clave privada ilegible"))?;
            store(dir, &key)?;
            secure_remove(&plain_path)?;
            tracing::info!("clave privada migrada a DPAPI ({KEY_FILE} → {KEY_FILE_DPAPI})");
            return Ok(Some(key));
        }
        Ok(None)
    }

    /// Cifra y guarda la clave. Verifica que el blob se descifra antes de
    /// darlo por bueno y escribe en un temporal que luego se renombra, para
    /// no dejar nunca un archivo a medias.
    pub(super) fn store(dir: &Path, key: &[u8]) -> Result<(), SecurityError> {
        let blob = dpapi::protect(key, DPAPI_ENTROPY, DPAPI_DESCRIPTION)?;
        if dpapi::unprotect(&blob, DPAPI_ENTROPY)? != key {
            return Err(SecurityError::InvalidIdentity(
                "DPAPI devolvió un blob que no se descifra a la clave original",
            ));
        }
        let tmp = dir.join(format!("{KEY_FILE_DPAPI}.tmp"));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&blob)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join(KEY_FILE_DPAPI))?;
        Ok(())
    }

    /// Sobrescribe el archivo con ceros, lo sincroniza y lo borra.
    fn secure_remove(path: &Path) -> io::Result<()> {
        let len = fs::metadata(path)?.len() as usize;
        {
            let mut f = fs::OpenOptions::new().write(true).open(path)?;
            f.write_all(&vec![0u8; len])?;
            f.sync_all()?;
        }
        fs::remove_file(path)
    }
}

/// Persistencia de la clave privada en archivo `0600` (Unix).
#[cfg(not(windows))]
mod key_store {
    use super::*;

    pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, SecurityError> {
        let path = dir.join(KEY_FILE);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read(path)?))
    }

    pub(super) fn store(dir: &Path, key: &[u8]) -> Result<(), SecurityError> {
        write_private(&dir.join(KEY_FILE), key)?;
        Ok(())
    }

    #[cfg(unix)]
    fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }

    #[cfg(not(unix))]
    fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
        fs::write(path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_reloads_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn distinct_devices_have_distinct_fingerprints() {
        assert_ne!(
            Identity::generate().unwrap().fingerprint(),
            Identity::generate().unwrap().fingerprint()
        );
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;

        #[test]
        fn key_is_stored_encrypted_with_dpapi() {
            let dir = tempfile::tempdir().unwrap();
            let a = Identity::load_or_create(dir.path()).unwrap();
            assert!(dir.path().join(KEY_FILE_DPAPI).exists());
            assert!(!dir.path().join(KEY_FILE).exists());
            let blob = fs::read(dir.path().join(KEY_FILE_DPAPI)).unwrap();
            // El PKCS#8 en claro no aparece en el archivo.
            assert!(!blob
                .windows(a.key_pkcs8.len())
                .any(|w| w == a.key_pkcs8.as_slice()));
            let b = Identity::load_or_create(dir.path()).unwrap();
            assert_eq!(a.fingerprint(), b.fingerprint());
            assert_eq!(a.key_pkcs8, b.key_pkcs8);
        }

        #[test]
        fn migrates_plaintext_key_and_removes_it() {
            let dir = tempfile::tempdir().unwrap();
            let old = Identity::generate().unwrap();
            fs::write(dir.path().join(CERT_FILE), old.cert_der.as_ref()).unwrap();
            fs::write(dir.path().join(KEY_FILE), &old.key_pkcs8).unwrap();

            let migrated = Identity::load_or_create(dir.path()).unwrap();
            assert_eq!(migrated.fingerprint(), old.fingerprint());
            assert_eq!(migrated.key_pkcs8, old.key_pkcs8);
            assert!(!dir.path().join(KEY_FILE).exists());
            assert!(dir.path().join(KEY_FILE_DPAPI).exists());

            let again = Identity::load_or_create(dir.path()).unwrap();
            assert_eq!(again.fingerprint(), old.fingerprint());
        }

        #[test]
        fn leftover_plaintext_is_removed_when_dpapi_exists() {
            let dir = tempfile::tempdir().unwrap();
            let a = Identity::load_or_create(dir.path()).unwrap();
            fs::write(dir.path().join(KEY_FILE), &a.key_pkcs8).unwrap();
            let b = Identity::load_or_create(dir.path()).unwrap();
            assert_eq!(a.fingerprint(), b.fingerprint());
            assert!(!dir.path().join(KEY_FILE).exists());
        }

        #[test]
        fn tampered_blob_is_a_clear_error_not_a_new_identity() {
            let dir = tempfile::tempdir().unwrap();
            let a = Identity::load_or_create(dir.path()).unwrap();
            let path = dir.path().join(KEY_FILE_DPAPI);
            let mut blob = fs::read(&path).unwrap();
            let mid = blob.len() / 2;
            blob[mid] ^= 0xFF;
            fs::write(&path, &blob).unwrap();

            let msg = Identity::load_or_create(dir.path())
                .unwrap_err()
                .to_string();
            assert!(msg.contains("DPAPI"), "mensaje poco claro: {msg}");
            // No se regeneró la identidad: el certificado sigue siendo el mismo.
            let cert = fs::read(dir.path().join(CERT_FILE)).unwrap();
            assert_eq!(cert.as_slice(), a.cert_der.as_ref());
            assert!(path.exists());
        }
    }
}
