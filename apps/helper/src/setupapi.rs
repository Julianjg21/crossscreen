//! Envoltura mínima de SetupAPI/newdev para dispositivos enumerados por raíz
//! (equivalente a `devcon install/remove`).
#![allow(unsafe_code)] // FFI Win32; cada bloque unsafe está acotado y comentado.

use anyhow::{Context, Result};
use std::path::Path;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{BOOL, HWND};

pub fn is_admin() -> bool {
    // SAFETY: sin argumentos ni punteros.
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

fn to_multi_sz(s: &str) -> Vec<u8> {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    w.extend([0, 0]);
    w.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// RAII para HDEVINFO.
struct DevInfo(HDEVINFO);
impl Drop for DevInfo {
    fn drop(&mut self) {
        // SAFETY: handle válido obtenido de SetupDi*; se destruye una sola vez.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Recorre todos los dispositivos (presentes o no) de la clase Display cuyo
/// hardware ID coincide, llamando a `f` con el conjunto y el elemento.
fn for_each_matching(
    hwid: &str,
    mut f: impl FnMut(HDEVINFO, &mut SP_DEVINFO_DATA) -> Result<()>,
) -> Result<()> {
    // SAFETY: GUID estático; flags válidos; el handle se libera con DevInfo.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            HWND::default(),
            DIGCF_PROFILE,
        )
    }
    .context("SetupDiGetClassDevsW")?;
    let set = DevInfo(set);
    let mut i = 0;
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `data` inicializado con cbSize correcto.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut data) }.is_err() {
            break;
        }
        i += 1;
        let mut buf = [0u8; 1024];
        let mut needed = 0u32;
        // SAFETY: buffer y longitud coherentes.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &data,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buf),
                Some(&mut needed),
            )
        };
        if ok.is_err() {
            continue;
        }
        let wide: Vec<u16> = buf[..needed as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let ids = String::from_utf16_lossy(&wide);
        if ids.split('\0').any(|id| id.eq_ignore_ascii_case(hwid)) {
            f(set.0, &mut data)?;
        }
    }
    Ok(())
}

/// Instance IDs de los dispositivos con ese hardware ID.
pub fn find_devices(hwid: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for_each_matching(hwid, |set, data| {
        let mut buf = [0u16; 512];
        let mut needed = 0u32;
        // SAFETY: buffer UTF-16 con su longitud.
        unsafe { SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buf), Some(&mut needed)) }?;
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        out.push(String::from_utf16_lossy(&buf[..len]));
        Ok(())
    })?;
    Ok(out)
}

/// Crea un nodo de dispositivo enumerado por raíz con el hardware ID dado.
pub fn create_root_device(hwid: &str) -> Result<()> {
    // SAFETY: GUID estático; handle liberado por DevInfo.
    let set = DevInfo(unsafe {
        SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), HWND::default())
    }?);
    let mut data = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    let class_name = HSTRING::from("Display");
    // SAFETY: cadenas válidas durante la llamada; `data` con cbSize correcto.
    unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            &class_name,
            &GUID_DEVCLASS_DISPLAY,
            PCWSTR::null(),
            HWND::default(),
            DICD_GENERATE_ID,
            Some(&mut data),
        )
    }
    .context("SetupDiCreateDeviceInfoW")?;
    let ids = to_multi_sz(hwid);
    // SAFETY: buffer REG_MULTI_SZ terminado en doble NUL.
    unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut data, SPDRP_HARDWAREID, Some(&ids)) }
        .context("SetupDiSetDeviceRegistryPropertyW(HARDWAREID)")?;
    // SAFETY: conjunto y elemento válidos.
    unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&data)) }
        .context("DIF_REGISTERDEVICE")?;
    Ok(())
}

/// Instala (o fuerza) el driver del INF para los dispositivos con ese hardware ID.
/// Devuelve si Windows pide reiniciar.
pub fn install_driver_for(hwid: &str, inf: &Path) -> Result<bool> {
    let inf = inf.canonicalize().context("ruta del INF")?;
    let inf_str = inf.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
    let mut reboot = BOOL(0);
    // SAFETY: cadenas válidas durante la llamada.
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            HWND::default(),
            &HSTRING::from(hwid),
            &HSTRING::from(inf_str.as_str()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )
    }
    .context("UpdateDriverForPlugAndPlayDevicesW")?;
    Ok(reboot.as_bool())
}

/// Elimina todos los dispositivos con ese hardware ID. Devuelve cuántos.
pub fn remove_devices(hwid: &str) -> Result<usize> {
    let mut n = 0;
    for_each_matching(hwid, |set, data| {
        // SAFETY: conjunto y elemento válidos.
        unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set, Some(data)) }.context("DIF_REMOVE")?;
        n += 1;
        Ok(())
    })?;
    // n == 0 no es error: puede no estar instalado.
    Ok(n)
}
