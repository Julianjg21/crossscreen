//! Topología con la API CCD de Windows (QueryDisplayConfig / SetDisplayConfig).
//!
//! Lecciones de la PoC-1 aplicadas aquí:
//! - Windows pone en *duplicar* cada monitor nuevo; `SDC_TOPOLOGY_EXTEND`
//!   activa **todos** los destinos. Por eso se construye la configuración con
//!   rutas explícitas: las activas actuales + la ruta del destino virtual con
//!   una fuente propia libre.
//! - El modo inicial que elige Windows es arbitrario (800×600@30); se fija
//!   después con `ChangeDisplaySettingsExW`.
#![allow(unsafe_code)] // FFI Win32; cada bloque unsafe está acotado y comentado.

use crate::{DisplayError, DisplayTarget, ShownDisplay, TargetKey, VDD_DEVICE_PATH_MARKER};
use cs_core::Mode;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::Graphics::Gdi::*;

const PATH_ACTIVE: u32 = 0x1; // DISPLAYCONFIG_PATH_ACTIVE
const MODE_IDX_INVALID: u32 = 0xFFFF_FFFF; // DISPLAYCONFIG_PATH_MODE_IDX_INVALID

struct Config {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

fn os_err(call: &'static str, code: i64) -> DisplayError {
    DisplayError::Os { call, code }
}

fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Config, DisplayError> {
    // La configuración puede cambiar entre GetDisplayConfigBufferSizes y
    // QueryDisplayConfig (ERROR_INSUFFICIENT_BUFFER): reintentar.
    for _ in 0..5 {
        let (mut np, mut nm) = (0u32, 0u32);
        // SAFETY: punteros a u32 válidos.
        let r = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if r != ERROR_SUCCESS {
            return Err(os_err("GetDisplayConfigBufferSizes", r.0 as i64));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: buffers con la capacidad anunciada en np/nm.
        let r = unsafe {
            QueryDisplayConfig(
                flags,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            )
        };
        if r == ERROR_SUCCESS {
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Ok(Config { paths, modes });
        }
        if r.0 != 122 {
            // 122 = ERROR_INSUFFICIENT_BUFFER
            return Err(os_err("QueryDisplayConfig", r.0 as i64));
        }
    }
    Err(os_err("QueryDisplayConfig", 122))
}

fn key_of(p: &DISPLAYCONFIG_PATH_INFO) -> TargetKey {
    TargetKey {
        adapter_low: p.targetInfo.adapterId.LowPart,
        adapter_high: p.targetInfo.adapterId.HighPart,
        target_id: p.targetInfo.id,
    }
}

fn target_name(adapter: LUID, id: u32) -> (String, String) {
    let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    req.header.adapterId = adapter;
    req.header.id = id;
    // SAFETY: cabecera inicializada con tipo y tamaño de la estructura completa.
    let r = unsafe { DisplayConfigGetDeviceInfo(&mut req.header) };
    if r != 0 {
        return (String::new(), String::new());
    }
    (
        wide_to_string(&req.monitorFriendlyDeviceName),
        wide_to_string(&req.monitorDevicePath),
    )
}

fn source_gdi_name(adapter: LUID, id: u32) -> Option<String> {
    let mut req = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
    req.header.adapterId = adapter;
    req.header.id = id;
    // SAFETY: cabecera inicializada con tipo y tamaño de la estructura completa.
    let r = unsafe { DisplayConfigGetDeviceInfo(&mut req.header) };
    (r == 0).then(|| wide_to_string(&req.viewGdiDeviceName))
}

fn wide_to_string(w: &[u16]) -> String {
    let len = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..len])
}

fn is_virtual_path(device_path: &str) -> bool {
    device_path
        .to_ascii_uppercase()
        .contains(VDD_DEVICE_PATH_MARKER)
}

pub fn targets() -> Result<Vec<DisplayTarget>, DisplayError> {
    let all = query(QDC_ALL_PATHS)?;
    let active = query(QDC_ONLY_ACTIVE_PATHS)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in all
        .paths
        .iter()
        .filter(|p| p.targetInfo.targetAvailable.as_bool())
    {
        let key = key_of(p);
        if !seen.insert(key) {
            continue;
        }
        let (friendly_name, device_path) = target_name(p.targetInfo.adapterId, p.targetInfo.id);
        let act = active.paths.iter().find(|a| key_of(a) == key);
        out.push(DisplayTarget {
            key,
            is_virtual: is_virtual_path(&device_path),
            friendly_name,
            device_path,
            active: act.is_some(),
            gdi_name: act.and_then(|a| source_gdi_name(a.sourceInfo.adapterId, a.sourceInfo.id)),
        });
    }
    Ok(out)
}

fn set_config(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
    what: &'static str,
) -> Result<(), DisplayError> {
    let flags =
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE;
    // SAFETY: slices válidos durante la llamada.
    let r = unsafe { SetDisplayConfig(Some(paths), Some(modes), flags) };
    const ERROR_ACCESS_DENIED: i32 = 5;
    if r == ERROR_ACCESS_DENIED {
        // Observado con la sesión bloqueada (pantalla de bloqueo activa).
        return Err(DisplayError::SessionNotInteractive);
    }
    if r != 0 {
        return Err(os_err(what, r as i64));
    }
    Ok(())
}

pub fn show(target: Option<TargetKey>, mode: Mode) -> Result<ShownDisplay, DisplayError> {
    let all = query(QDC_ALL_PATHS)?;
    let current = query(QDC_ONLY_ACTIVE_PATHS)?;

    // Si ya está activo, solo ajustar el modo.
    let virtual_keys: Vec<TargetKey> = targets()?
        .into_iter()
        .filter(|t| t.is_virtual && target.is_none_or(|k| k == t.key))
        .map(|t| t.key)
        .collect();
    if virtual_keys.is_empty() {
        return Err(DisplayError::NoVirtualTarget);
    }

    let mut paths = current.paths.clone();
    // Quitar rutas en las que un destino virtual clona otra fuente (estado en
    // que Windows deja los monitores nuevos): se recrearán con fuente propia.
    let clone_sources: HashSet<(i32, u32, u32)> = paths
        .iter()
        .filter(|p| !virtual_keys.contains(&key_of(p)))
        .map(|p| {
            (
                p.sourceInfo.adapterId.HighPart,
                p.sourceInfo.adapterId.LowPart,
                p.sourceInfo.id,
            )
        })
        .collect();
    paths.retain(|p| {
        let k = key_of(p);
        let src = (
            p.sourceInfo.adapterId.HighPart,
            p.sourceInfo.adapterId.LowPart,
            p.sourceInfo.id,
        );
        !(virtual_keys.contains(&k) && clone_sources.contains(&src))
    });

    let chosen = virtual_keys[0];
    if !paths.iter().any(|p| key_of(p) == chosen) {
        let used_sources: HashSet<(i32, u32, u32)> = paths
            .iter()
            .map(|p| {
                (
                    p.sourceInfo.adapterId.HighPart,
                    p.sourceInfo.adapterId.LowPart,
                    p.sourceInfo.id,
                )
            })
            .collect();
        // Ruta candidata: mismo destino, fuente en el mismo adaptador y libre.
        let mut cand = *all
            .paths
            .iter()
            .find(|p| {
                key_of(p) == chosen
                    && p.sourceInfo.adapterId.LowPart == p.targetInfo.adapterId.LowPart
                    && p.sourceInfo.adapterId.HighPart == p.targetInfo.adapterId.HighPart
                    && !used_sources.contains(&(
                        p.sourceInfo.adapterId.HighPart,
                        p.sourceInfo.adapterId.LowPart,
                        p.sourceInfo.id,
                    ))
            })
            .ok_or(DisplayError::NoVirtualTarget)?;
        cand.flags |= PATH_ACTIVE;
        // Índices de modo inválidos: Windows elige; el modo se fija después.
        cand.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        cand.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        paths.push(cand);
    }
    set_config(&paths, &current.modes, "SetDisplayConfig(show)")?;

    // Esperar a que aparezca con nombre GDI.
    let deadline = Instant::now() + Duration::from_secs(5);
    let gdi_name = loop {
        if let Some(t) = targets()?.into_iter().find(|t| t.key == chosen && t.active) {
            if let Some(n) = t.gdi_name {
                break n;
            }
        }
        if Instant::now() > deadline {
            return Err(DisplayError::NotActivated);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    set_mode(&gdi_name, mode)?;
    Ok(ShownDisplay {
        key: chosen,
        gdi_name,
        mode,
    })
}

pub fn hide(target: TargetKey) -> Result<(), DisplayError> {
    let mut current = query(QDC_ONLY_ACTIVE_PATHS)?;
    let mut changed = false;
    for p in current.paths.iter_mut().filter(|p| key_of(p) == target) {
        p.flags &= !PATH_ACTIVE;
        changed = true;
    }
    if !changed {
        return Ok(()); // ya retirado
    }
    set_config(&current.paths, &current.modes, "SetDisplayConfig(hide)")
}

fn devmode() -> DEVMODEW {
    DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    }
}

pub fn modes(gdi_name: &str) -> Vec<Mode> {
    let name: Vec<u16> = gdi_name.encode_utf16().chain([0]).collect();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let mut dm = devmode();
        // SAFETY: nombre terminado en NUL; DEVMODEW con dmSize correcto.
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(name.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        };
        if !ok.as_bool() {
            break;
        }
        let m = Mode::new(dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency);
        if !out.contains(&m) {
            out.push(m);
        }
        i += 1;
    }
    out
}

fn set_mode(gdi_name: &str, mode: Mode) -> Result<(), DisplayError> {
    let hz = mode.refresh_mhz / 1000;
    if !modes(gdi_name).contains(&Mode::new(mode.width, mode.height, hz)) {
        return Err(DisplayError::ModeNotAvailable(mode));
    }
    let name: Vec<u16> = gdi_name.encode_utf16().chain([0]).collect();
    let mut dm = devmode();
    dm.dmPelsWidth = mode.width;
    dm.dmPelsHeight = mode.height;
    dm.dmDisplayFrequency = hz;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    // SAFETY: nombre terminado en NUL; DEVMODEW inicializado.
    let r = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if r != DISP_CHANGE_SUCCESSFUL {
        return Err(os_err("ChangeDisplaySettingsExW", r.0 as i64));
    }
    Ok(())
}
