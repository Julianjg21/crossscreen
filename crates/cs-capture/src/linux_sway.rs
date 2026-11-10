//! Monitor virtual en Sway (ADR-0011): una salida *headless* creada con
//! `swaymsg create_output`, con la resolución del receptor y colocada a la
//! derecha de las salidas activas. Se retira con `output <nombre> unplug`.
//!
//! Se usa `swaymsg` (incluido con Sway) en vez de hablar el protocolo IPC
//! a mano: es la interfaz documentada y estable.

use std::process::Command;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SwayError {
    #[error("no es una sesión de Sway (falta SWAYSOCK) o no se encontró swaymsg")]
    NotAvailable,
    #[error("swaymsg {args}: {msg}")]
    Command { args: String, msg: String },
    #[error("Sway no creó la salida virtual")]
    NotCreated,
}

/// Salida de Sway según `swaymsg -t get_outputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwayOutputInfo {
    pub name: String,
    pub active: bool,
    pub x: i64,
    pub width: i64,
}

/// Salida virtual creada por CrossScreen. Se retira al soltarla.
#[derive(Debug)]
pub struct SwayVirtualOutput {
    name: String,
    removed: bool,
}

fn swaymsg(args: &[&str]) -> Result<String, SwayError> {
    let out = Command::new("swaymsg")
        .args(args)
        .output()
        .map_err(|_| SwayError::NotAvailable)?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let msg = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).trim().to_owned()
        } else {
            stdout.trim().to_owned()
        };
        return Err(SwayError::Command {
            args: args.join(" "),
            msg,
        });
    }
    Ok(stdout)
}

/// ¿Estamos en una sesión de Sway con `swaymsg` disponible?
pub fn is_available() -> bool {
    std::env::var_os("SWAYSOCK").is_some() && swaymsg(&["-t", "get_version"]).is_ok()
}

/// Interpreta la salida JSON de `swaymsg -t get_outputs -r`.
pub fn parse_outputs(json: &str) -> Vec<SwayOutputInfo> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|o| {
            let rect = o.get("rect");
            Some(SwayOutputInfo {
                name: o.get("name")?.as_str()?.to_owned(),
                active: o.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
                x: rect
                    .and_then(|r| r.get("x"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                width: rect
                    .and_then(|r| r.get("width"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            })
        })
        .collect()
}

fn outputs() -> Result<Vec<SwayOutputInfo>, SwayError> {
    Ok(parse_outputs(&swaymsg(&["-t", "get_outputs", "-r"])?))
}

/// ¿Existe la salida `name`? Ante un error de `swaymsg` responde que sí
/// (no cortar la sesión por un fallo pasajero). Bloquea.
pub fn output_exists(name: &str) -> bool {
    outputs()
        .map(|v| v.iter().any(|o| o.name == name))
        .unwrap_or(true)
}

/// Posición x a la derecha de todas las salidas activas.
pub fn right_edge(outputs: &[SwayOutputInfo], except: &str) -> i64 {
    outputs
        .iter()
        .filter(|o| o.active && o.name != except)
        .map(|o| o.x + o.width)
        .max()
        .unwrap_or(0)
}

impl SwayVirtualOutput {
    /// Crea la salida virtual `width`x`height`@`hz`. Bloquea (lanza
    /// `swaymsg`): llamar desde `spawn_blocking`.
    pub fn create(width: u32, height: u32, hz: u32) -> Result<Self, SwayError> {
        let before: Vec<String> = outputs()?.into_iter().map(|o| o.name).collect();
        swaymsg(&["create_output"])?;
        // La salida nueva aparece al momento; se reintenta por si tarda.
        let mut name = None;
        for _ in 0..20 {
            if let Some(o) = outputs()?.into_iter().find(|o| !before.contains(&o.name)) {
                name = Some(o.name);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let name = name.ok_or(SwayError::NotCreated)?;
        let out = Self {
            name,
            removed: false,
        };
        // Si algo falla a partir de aquí, `Drop` la retira.
        let mode = format!("{width}x{height}@{hz}Hz");
        // `--`: que swaymsg no tome `--custom` por una opción suya.
        swaymsg(&["--", "output", &out.name, "mode", "--custom", &mode])?;
        let x = right_edge(&outputs()?, &out.name);
        swaymsg(&["output", &out.name, "pos", &x.to_string(), "0"])?;
        tracing::info!(output = %out.name, %mode, x, "salida virtual de Sway creada");
        Ok(out)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Retira la salida (bloquea: `spawn_blocking`).
    pub fn remove(mut self) {
        self.unplug();
    }

    fn unplug(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        match swaymsg(&["output", &self.name, "unplug"]) {
            Ok(_) => tracing::info!(output = %self.name, "salida virtual de Sway retirada"),
            Err(e) => tracing::warn!("no se pudo retirar la salida {}: {e}", self.name),
        }
    }

    /// ¿Sigue existiendo la salida? (el usuario pudo retirarla).
    pub fn exists(&self) -> bool {
        output_exists(&self.name)
    }
}

impl Drop for SwayVirtualOutput {
    fn drop(&mut self) {
        self.unplug();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"name": "eDP-1", "active": true, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}},
      {"name": "HDMI-A-1", "active": true, "rect": {"x": 1920, "y": 0, "width": 2560, "height": 1440}},
      {"name": "DP-2", "active": false, "rect": {"x": 0, "y": 0, "width": 0, "height": 0}},
      {"name": "HEADLESS-1", "active": true, "rect": {"x": 9000, "y": 0, "width": 1366, "height": 768}}
    ]"#;

    #[test]
    fn outputs_are_parsed() {
        let o = parse_outputs(SAMPLE);
        assert_eq!(o.len(), 4);
        assert_eq!(o[1].name, "HDMI-A-1");
        assert_eq!((o[1].x, o[1].width), (1920, 2560));
        assert!(!o[2].active);
        assert!(parse_outputs("no es json").is_empty());
    }

    #[test]
    fn new_output_goes_to_the_right_of_the_others() {
        let o = parse_outputs(SAMPLE);
        // Sin contar la propia salida virtual ni las inactivas.
        assert_eq!(right_edge(&o, "HEADLESS-1"), 4480);
        assert_eq!(right_edge(&[], "x"), 0);
    }
}
