//! Arranque del agente al iniciar sesión (sin privilegios: solo el usuario).
//!
//! - Windows: valor `CrossScreen` en `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! - Linux: `~/.config/autostart/crossscreen-agent.desktop` (XDG autostart,
//!   lo respetan GNOME, KDE y la mayoría de escritorios).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// Ejecutable del agente que se registrará.
fn agent_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("ruta del agente")?;
    // Un AppImage se monta en una ruta temporal distinta en cada arranque:
    // una entrada de inicio apuntaría a algo que ya no existe.
    #[cfg(not(windows))]
    if std::env::var_os("APPIMAGE").is_some() || exe.starts_with("/tmp/.mount_") {
        bail!("con el AppImage no se puede arrancar con el sistema; instala el .deb");
    }
    Ok(exe)
}

/// ¿Se puede activar en esta instalación?
pub fn supported() -> bool {
    agent_exe().is_ok()
}

#[cfg(windows)]
#[allow(unsafe_code)] // FFI del registro de Win32; cada bloque unsafe está acotado y comentado.
mod imp {
    use super::*;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "CrossScreen";

    pub fn is_enabled() -> bool {
        // SAFETY: solo se consulta si el valor existe (sin búfer de datos).
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(KEY),
                &HSTRING::from(VALUE),
                RRF_RT_REG_SZ,
                None,
                None,
                None,
            )
        };
        r == ERROR_SUCCESS
    }

    pub fn set(enabled: bool) -> Result<()> {
        if enabled {
            let exe = agent_exe()?;
            let cmd = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` es una cadena UTF-16 terminada en NUL que vive
            // durante la llamada; el tamaño se pasa en bytes.
            let r = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    &HSTRING::from(KEY),
                    &HSTRING::from(VALUE),
                    REG_SZ.0,
                    Some(wide.as_ptr().cast()),
                    (wide.len() * 2) as u32,
                )
            };
            if r != ERROR_SUCCESS {
                bail!("no se pudo registrar el inicio automático (código {})", r.0);
            }
        } else {
            // SAFETY: borra un valor por nombre en la clave del usuario.
            let r = unsafe {
                RegDeleteKeyValueW(
                    HKEY_CURRENT_USER,
                    &HSTRING::from(KEY),
                    &HSTRING::from(VALUE),
                )
            };
            if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
                bail!("no se pudo quitar el inicio automático (código {})", r.0);
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    fn entry_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart").join("crossscreen-agent.desktop"))
    }

    /// Contenido de la entrada XDG para `exe`.
    pub(super) fn desktop_entry(exe: &std::path::Path) -> String {
        // Exec: comillas dobles y escapes según la especificación de
        // Desktop Entry, por si la ruta tiene espacios.
        let quoted = exe
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        format!(
            "[Desktop Entry]\nType=Application\nName=CrossScreen (agente)\nComment=Servicio de CrossScreen en segundo plano\nExec=\"{quoted}\"\nNoDisplay=true\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        )
    }

    pub fn is_enabled() -> bool {
        entry_path().is_some_and(|p| p.is_file())
    }

    pub fn set(enabled: bool) -> Result<()> {
        let path = entry_path().context("no se pudo determinar ~/.config")?;
        if enabled {
            let exe = agent_exe()?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creando {}", dir.display()))?;
            }
            std::fs::write(&path, desktop_entry(&exe))
                .with_context(|| format!("escribiendo {}", path.display()))?;
        } else if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("borrando {}", path.display()))?;
        }
        Ok(())
    }
}

pub use imp::{is_enabled, set};

#[cfg(all(test, not(windows)))]
mod tests {
    use super::imp::desktop_entry;

    /// Escribe en `$XDG_CONFIG_HOME`: ejecutar con una carpeta temporal
    /// (`XDG_CONFIG_HOME=/tmp/x cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn autostart_entry_roundtrip() {
        super::set(true).unwrap();
        assert!(super::is_enabled());
        super::set(false).unwrap();
        assert!(!super::is_enabled());
        super::set(false).unwrap();
    }

    #[test]
    fn desktop_entry_quotes_exec() {
        let e = desktop_entry(std::path::Path::new("/opt/Cross Screen/crossscreen-agent"));
        assert!(e.contains("Exec=\"/opt/Cross Screen/crossscreen-agent\"\n"));
        assert!(e.starts_with("[Desktop Entry]\n"));
        let e = desktop_entry(std::path::Path::new("/x/a$b\"c"));
        assert!(e.contains(r#"Exec="/x/a\$b\"c""#));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    /// Toca el registro real del usuario (clave Run): se ejecuta a mano con
    /// `cargo test -p crossscreen-agent -- --ignored` y deja todo como estaba.
    #[test]
    #[ignore]
    fn run_key_roundtrip() {
        let before = super::is_enabled();
        super::set(true).unwrap();
        assert!(super::is_enabled());
        super::set(false).unwrap();
        assert!(!super::is_enabled());
        // Borrar dos veces no falla.
        super::set(false).unwrap();
        if before {
            super::set(true).unwrap();
        }
    }
}
