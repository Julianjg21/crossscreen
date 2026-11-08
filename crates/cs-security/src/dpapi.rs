//! Envoltorio mínimo de DPAPI (`CryptProtectData` / `CryptUnprotectData`).
//!
//! Ámbito: usuario actual (sin `CRYPTPROTECT_LOCAL_MACHINE`), así que solo la
//! misma cuenta de Windows puede descifrar el blob. Se pasa una entropía
//! adicional fija para que otro programa del mismo usuario que llame a
//! `CryptUnprotectData` sin ella no obtenga la clave "por accidente" (no es un
//! secreto: protege contra uso indebido, no contra malware del propio usuario).

#![allow(unsafe_code)]

use std::io;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob(data: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "datos demasiado grandes"))?;
    Ok(CRYPT_INTEGER_BLOB {
        cbData: len,
        // DPAPI declara el puntero como mutable pero no escribe en la entrada.
        pbData: data.as_ptr() as *mut u8,
    })
}

/// Copia el resultado devuelto por DPAPI y libera su memoria con `LocalFree`.
fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: tras una llamada correcta, DPAPI garantiza que `pbData` apunta a
    // `cbData` bytes válidos reservados con `LocalAlloc`; se copian antes de
    // liberar y el puntero no se vuelve a usar.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        // Antes de liberar, borrar la copia de DPAPI (puede ser texto en claro).
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        let _ = LocalFree(HLOCAL(out.pbData.cast()));
        v
    }
}

/// Cifra `data` para el usuario actual.
pub(crate) fn protect(data: &[u8], entropy: &[u8], description: &str) -> io::Result<Vec<u8>> {
    let input = blob(data)?;
    let entropy = blob(entropy)?;
    let descr: Vec<u16> = description.encode_utf16().chain(Some(0)).collect();
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input`, `entropy` y `descr` apuntan a memoria válida que vive
    // hasta el final de la llamada; `descr` termina en NUL; `out` es una
    // salida válida que DPAPI rellena y que `take_output` libera.
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR(descr.as_ptr()),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    }
    .map_err(|e| io::Error::other(format!("CryptProtectData falló: {e}")))?;
    Ok(take_output(out))
}

/// Descifra un blob producido por [`protect`] con la misma entropía.
pub(crate) fn unprotect(blob_data: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(blob_data)?;
    let entropy = blob(entropy)?;
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: igual que en `protect`; no se pide la descripción (`None`).
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    }
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "no se pudo descifrar la clave privada con DPAPI \
                 (archivo alterado, de otro usuario o de otro equipo): {e}"
            ),
        )
    })?;
    Ok(take_output(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_entropy() {
        let blob = protect(b"secreto", b"e1", "prueba").unwrap();
        assert!(!blob.windows(7).any(|w| w == b"secreto"));
        assert_eq!(unprotect(&blob, b"e1").unwrap(), b"secreto");
        assert!(unprotect(&blob, b"e2").is_err());
    }
}
