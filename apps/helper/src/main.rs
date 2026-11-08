//! `crossscreen-helper`: única pieza de CrossScreen que se ejecuta con
//! privilegios de administrador, bajo demanda (UAC), nunca como servicio.
//!
//! Superficie mínima y argumentos validados (ver `docs/02-arquitectura.md` D2):
//! - `vdd-install --from <dir>`: verifica cada archivo contra el manifiesto
//!   fijado (`drivers/windows/vdd-manifest.toml`), prepara el directorio de
//!   configuración del driver y crea/instala el dispositivo `Root\MttVDD`.
//! - `vdd-uninstall`: elimina el dispositivo y el paquete del almacén de drivers.
//! - `vdd-restart`: reinicia el dispositivo (aplica cambios de configuración).
//!
//! Prohibido por diseño: certificados raíz, tareas programadas como SYSTEM,
//! reglas que permitan repetir acciones privilegiadas sin consentimiento.

#[cfg(windows)]
mod setupapi;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = include_str!("../../../drivers/windows/vdd-manifest.toml");
/// SID de LOCAL SERVICE: cuenta del host UMDF (WUDFHost) que ejecuta el driver.
const LOCAL_SERVICE_SID: &str = "S-1-5-19";

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    hardware_id: String,
    config_dir: String,
    files: BTreeMap<String, String>,
}

#[derive(Parser)]
#[command(name = "crossscreen-helper", version)]
struct Cli {
    /// Archivo donde escribir el registro (útil al lanzarse elevado vía UAC,
    /// porque la consola elevada no es visible para el proceso que lo lanzó).
    #[arg(long, global = true)]
    log: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::enum_variant_names)] // los subcomandos públicos son `vdd-*` a propósito
enum Cmd {
    VddInstall {
        #[arg(long)]
        from: PathBuf,
    },
    VddUninstall {
        /// Borrar también el directorio de configuración del driver.
        #[arg(long)]
        purge_config: bool,
    },
    VddRestart,
    VddStatus,
}

struct Log(Option<fs::File>);

impl Log {
    fn line(&mut self, s: impl AsRef<str>) {
        println!("{}", s.as_ref());
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{}", s.as_ref());
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let mut log = Log(cli.log.as_ref().and_then(|p| fs::File::create(p).ok()));
    let result = run(cli.cmd, &mut log);
    match result {
        Ok(()) => log.line("OK"),
        Err(e) => {
            log.line(format!("ERROR: {e:#}"));
            std::process::exit(1);
        }
    }
}

fn run(cmd: Cmd, log: &mut Log) -> Result<()> {
    let manifest: Manifest = toml::from_str(MANIFEST).context("manifiesto inválido")?;
    match cmd {
        Cmd::VddStatus => status(&manifest, log),
        Cmd::VddInstall { from } => {
            require_admin()?;
            install(&manifest, &from, log)
        }
        Cmd::VddUninstall { purge_config } => {
            require_admin()?;
            uninstall(&manifest, purge_config, log)
        }
        Cmd::VddRestart => {
            require_admin()?;
            restart(&manifest, log)
        }
    }
}

fn require_admin() -> Result<()> {
    #[cfg(windows)]
    if !setupapi::is_admin() {
        bail!("se requieren privilegios de administrador");
    }
    #[cfg(not(windows))]
    bail!("solo disponible en Windows");
    #[cfg(windows)]
    Ok(())
}

fn sha256_file(p: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        fs::read(p).with_context(|| format!("leyendo {}", p.display()))?,
    )))
}

/// Verifica que `dir` contiene exactamente los archivos fijados con su hash.
fn verify_package(m: &Manifest, dir: &Path, log: &mut Log) -> Result<PathBuf> {
    for (name, expected) in &m.files {
        let got = sha256_file(&dir.join(name))?;
        if &got != expected {
            bail!("hash de {name} no coincide con el manifiesto (esperado {expected}, obtenido {got})");
        }
        log.line(format!("  verificado {name}"));
    }
    Ok(dir.join("MttVDD.inf"))
}

/// Crea el directorio de configuración con permisos acotados:
/// el usuario que instala (modificar) y LOCAL SERVICE (modificar: el driver
/// escribe logs y actualiza `count`). Sin acceso para otros usuarios.
fn prepare_config_dir(m: &Manifest, pkg: &Path, log: &mut Log) -> Result<()> {
    let dir = Path::new(&m.config_dir);
    fs::create_dir_all(dir)?;
    let settings = dir.join("vdd_settings.xml");
    if !settings.exists() {
        fs::copy(pkg.join("vdd_settings.xml"), &settings)?;
        log.line(format!("  creado {}", settings.display()));
    }
    let user_sid = current_user_sid()?;
    let status = Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("*{user_sid}:(OI)(CI)M"))
        .arg(format!("*{LOCAL_SERVICE_SID}:(OI)(CI)M"))
        .args(["*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/Q"])
        .status()?;
    if !status.success() {
        bail!("icacls falló al ajustar permisos de {}", dir.display());
    }
    log.line(format!("  permisos: usuario {user_sid} + LOCAL SERVICE (modificar), SYSTEM/Administradores (total)"));
    Ok(())
}

fn current_user_sid() -> Result<String> {
    let out = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let sid = s
        .trim()
        .rsplit(',')
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .to_owned();
    if !sid.starts_with("S-1-") {
        bail!("no se pudo obtener el SID del usuario: {s}");
    }
    Ok(sid)
}

fn install(m: &Manifest, from: &Path, log: &mut Log) -> Result<()> {
    log.line(format!("Instalando Virtual-Display-Driver {}", m.version));
    let inf = verify_package(m, from, log)?;
    prepare_config_dir(m, from, log)?;
    #[cfg(windows)]
    {
        let existing = setupapi::find_devices(&m.hardware_id)?;
        if existing.is_empty() {
            setupapi::create_root_device(&m.hardware_id)?;
            log.line("  creado nodo de dispositivo Root\\MttVDD");
        } else {
            log.line(format!(
                "  ya existe el dispositivo ({}); se actualiza el driver",
                existing.len()
            ));
        }
        let reboot = setupapi::install_driver_for(&m.hardware_id, &inf)?;
        log.line(format!("  driver instalado (reinicio requerido: {reboot})"));
    }
    Ok(())
}

fn uninstall(m: &Manifest, purge: bool, log: &mut Log) -> Result<()> {
    #[cfg(windows)]
    {
        let n = setupapi::remove_devices(&m.hardware_id)?;
        log.line(format!("  eliminados {n} dispositivo(s)"));
    }
    for oem in published_names_for("mttvdd.inf")? {
        let st = Command::new("pnputil")
            .args(["/delete-driver", &oem, "/uninstall", "/force"])
            .status()?;
        log.line(format!(
            "  pnputil /delete-driver {oem}: {}",
            if st.success() { "ok" } else { "falló" }
        ));
    }
    if purge {
        let _ = fs::remove_dir_all(&m.config_dir);
        log.line(format!("  borrado {}", m.config_dir));
    }
    Ok(())
}

fn restart(m: &Manifest, log: &mut Log) -> Result<()> {
    #[cfg(windows)]
    for id in setupapi::find_devices(&m.hardware_id)? {
        let st = Command::new("pnputil")
            .args(["/restart-device", &id])
            .status()?;
        log.line(format!("  reiniciado {id}: {}", st.success()));
    }
    Ok(())
}

fn status(m: &Manifest, log: &mut Log) -> Result<()> {
    #[cfg(windows)]
    for id in setupapi::find_devices(&m.hardware_id)? {
        log.line(format!("dispositivo: {id}"));
    }
    for oem in published_names_for("mttvdd.inf")? {
        log.line(format!("paquete en el almacén: {oem}"));
    }
    Ok(())
}

/// Nombres publicados (`oemNN.inf`) de un paquete en el almacén de drivers.
fn published_names_for(original: &str) -> Result<Vec<String>> {
    let out = Command::new("pnputil").args(["/enum-drivers"]).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let Some((_, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.to_ascii_lowercase().starts_with("oem")
            && value.to_ascii_lowercase().ends_with(".inf")
        {
            current = Some(value.to_owned());
        } else if value.eq_ignore_ascii_case(original) {
            if let Some(c) = current.take() {
                result.push(c);
            }
        }
    }
    Ok(result)
}
