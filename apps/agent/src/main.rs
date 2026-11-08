//! `crossscreen-agent`: daemon de usuario de CrossScreen.
//!
//! Posee la identidad del dispositivo, el almacén de confianza, el anuncio y
//! la exploración mDNS, el emparejamiento y la gestión del monitor virtual, y
//! los expone a la UI por IPC local (`cs-ipc`). Ver `docs/02-arquitectura.md` (D2).
//!
//! Una sola instancia por usuario: si el punto IPC ya existe, sale con código 2.
//! `--probe` actúa como cliente de diagnóstico de un agente ya en ejecución.

mod agent;
mod probe;

use anyhow::{Context, Result};
use clap::Parser;
use cs_ipc::{Endpoint, Listener};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "crossscreen-agent",
    version,
    about = "CrossScreen: agente en segundo plano"
)]
struct Args {
    /// Directorio de datos (identidad y pares de confianza). Por defecto, el
    /// mismo que la CLI.
    #[arg(long, env = "CROSSSCREEN_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Nombre visible de este equipo.
    #[arg(long)]
    name: Option<String>,
    /// No ejecutar el agente: conectarse al que está en marcha y mostrar su
    /// estado, identidad, pares y monitores (diagnóstico).
    #[arg(long)]
    probe: bool,
    /// Filtro de registro (sintaxis de `RUST_LOG`).
    #[arg(long, default_value = "info")]
    log: String,
}

fn main() {
    let args = Args::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("No se pudo iniciar el runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(async {
        let res = if args.probe {
            probe::run().await
        } else {
            run(args).await
        };
        match res {
            Ok(code) => code,
            Err(e) => {
                tracing::error!("{e:#}");
                eprintln!("Error: {e:#}");
                1
            }
        }
    });
    std::process::exit(code);
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "CrossScreen".into())
}

fn init_logging(dir: &std::path::Path, filter: &str) -> Result<PathBuf> {
    let path = dir.join("agent.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("abriendo el registro {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(filter)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    Ok(path)
}

async fn run(args: Args) -> Result<i32> {
    let dir = match args.data_dir {
        Some(d) => d,
        None => directories::ProjectDirs::from("org", "CrossScreen", "CrossScreen")
            .context("no se pudo determinar el directorio de datos")?
            .data_dir()
            .to_path_buf(),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creando el directorio de datos {}", dir.display()))?;
    let log_file = init_logging(&dir, &args.log)?;

    let ep = Endpoint::default_for_user().context("determinando el punto IPC")?;
    // Primero el punto IPC: garantiza la instancia única antes de tocar nada más.
    let mut listener = match Listener::bind(&ep).await {
        Ok(l) => l,
        Err(e) if cs_ipc::transport::is_name_in_use(&e) => {
            let msg =
                format!("Ya hay un agente de CrossScreen en ejecución para este usuario ({ep}).");
            tracing::warn!("{msg}");
            eprintln!("{msg}");
            return Ok(2);
        }
        Err(e) => return Err(e).context("creando el punto IPC"),
    };

    let name = args.name.unwrap_or_else(default_name);
    let agent = agent::Agent::start(dir.clone(), name, Some(log_file.clone()))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        endpoint = %ep,
        data_dir = %dir.display(),
        "agente iniciado"
    );
    println!("Agente de CrossScreen en ejecución. IPC: {ep}");
    println!("Registro: {}", log_file.display());

    let accept_loop = {
        let agent = agent.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        let agent = agent.clone();
                        let events = agent.subscribe();
                        let version = cs_ipc::agent_version_string();
                        tokio::spawn(async move {
                            cs_ipc::serve_connection(stream, agent, events, &version).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("error aceptando conexión IPC: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    };

    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            // Sin consola no hay Ctrl+C: seguir funcionando hasta que se mate el proceso.
            tracing::warn!("no se pudo esperar Ctrl+C: {e}");
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = accept_loop => {}
        () = ctrl_c => {}
    }
    tracing::info!("agente detenido");
    agent.shutdown().await;
    drop::<Arc<agent::Agent>>(agent);
    Ok(0)
}
