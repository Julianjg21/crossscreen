//! `crossscreen display …`: gestión de monitores virtuales (topología y modo).
//! No crea ni configura el driver (eso es del helper con privilegios).

use anyhow::{bail, Result};
use clap::Subcommand;
use cs_core::Mode;
use cs_display::Displays;
use std::time::Instant;

#[derive(Subcommand)]
pub enum DisplayCmd {
    /// Lista los monitores conectados (físicos y virtuales).
    List,
    /// Muestra un monitor virtual extendiendo el escritorio.
    Show {
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 60)]
        hz: u32,
    },
    /// Retira del escritorio todos los monitores virtuales activos.
    Hide,
    /// Ciclos de mostrar/retirar para medir tiempos y estabilidad (PoC-1).
    Cycle {
        #[arg(long, default_value_t = 100)]
        n: u32,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 60)]
        hz: u32,
    },
}

pub fn run(cmd: DisplayCmd) -> Result<()> {
    let d = Displays::new();
    match cmd {
        DisplayCmd::List => {
            for t in d.targets()? {
                println!(
                    "{} {:<28} activo={:<5} {:<14} {}",
                    if t.is_virtual {
                        "[virtual]"
                    } else {
                        "[físico] "
                    },
                    if t.friendly_name.is_empty() {
                        "(panel interno)"
                    } else {
                        &t.friendly_name
                    },
                    t.active,
                    t.gdi_name.as_deref().unwrap_or("-"),
                    t.device_path
                );
                if let Some(g) = t.gdi_name.as_deref().filter(|_| t.is_virtual) {
                    let modes: Vec<String> = d
                        .modes(g)?
                        .iter()
                        .map(|m| format!("{}x{}@{}", m.width, m.height, m.refresh_mhz / 1000))
                        .collect();
                    println!("          modos: {}", modes.join(" "));
                }
            }
        }
        DisplayCmd::Show { width, height, hz } => {
            let t0 = Instant::now();
            let s = d.show(None, Mode::new(width, height, hz))?;
            println!(
                "Mostrado {} a {width}x{height}@{hz} en {:?}",
                s.gdi_name,
                t0.elapsed()
            );
        }
        DisplayCmd::Hide => {
            let mut n = 0;
            for t in d
                .targets()?
                .into_iter()
                .filter(|t| t.is_virtual && t.active)
            {
                d.hide(t.key)?;
                n += 1;
            }
            println!("Retirados {n} monitor(es) virtual(es)");
        }
        DisplayCmd::Cycle {
            n,
            width,
            height,
            hz,
        } => {
            let mode = Mode::new(width, height, hz);
            let (mut show_ms, mut hide_ms, mut failures) = (Vec::new(), Vec::new(), 0u32);
            for i in 1..=n {
                let t0 = Instant::now();
                let shown = match d.show(None, mode) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("ciclo {i}: show falló: {e}");
                        failures += 1;
                        continue;
                    }
                };
                show_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                let t1 = Instant::now();
                if let Err(e) = d.hide(shown.key) {
                    eprintln!("ciclo {i}: hide falló: {e}");
                    failures += 1;
                    continue;
                }
                // Confirmar que realmente se retiró.
                if d.targets()?.iter().any(|t| t.key == shown.key && t.active) {
                    eprintln!("ciclo {i}: el monitor sigue activo tras hide");
                    failures += 1;
                }
                hide_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
            }
            let stats = |v: &mut Vec<f64>| -> String {
                if v.is_empty() {
                    return "-".into();
                }
                v.sort_by(|a, b| a.total_cmp(b));
                let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
                format!(
                    "p50 {:.0} ms / p95 {:.0} ms / máx {:.0} ms",
                    p(0.5),
                    p(0.95),
                    p(1.0)
                )
            };
            println!("Ciclos: {n}, fallos: {failures}");
            println!("Mostrar (topología + modo): {}", stats(&mut show_ms));
            println!("Retirar:                    {}", stats(&mut hide_ms));
            if failures > 0 {
                bail!("{failures} fallos");
            }
        }
    }
    Ok(())
}
