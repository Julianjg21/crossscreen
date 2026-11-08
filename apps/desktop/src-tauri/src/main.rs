//! Panel de control de CrossScreen.
//!
//! Capa fina (ver `docs/05-estructura-monorepo.md`): los comandos solo
//! reenvían peticiones al agente (`crossscreen-agent`) por `cs-ipc` y
//! reemiten sus eventos al frontend. No hay lógica de negocio aquí.

// En release no abrir una consola junto a la ventana.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cs_ipc::{Endpoint, IpcClient, IpcClientError, Request, Response};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

#[derive(Default)]
struct AgentConn {
    client: Mutex<Option<Arc<IpcClient>>>,
}

/// Error que ve el frontend.
#[derive(Debug, Serialize)]
struct CmdError {
    /// `agent_unavailable` | `agent_error` | `protocol`
    kind: &'static str,
    /// Código del agente (`not_found`, `busy`, …) si `kind == "agent_error"`.
    code: Option<cs_ipc::ErrorCode>,
    message: String,
}

impl CmdError {
    fn unavailable(message: impl Into<String>) -> Self {
        CmdError {
            kind: "agent_unavailable",
            code: None,
            message: message.into(),
        }
    }
}

impl From<IpcClientError> for CmdError {
    fn from(e: IpcClientError) -> Self {
        match e {
            IpcClientError::Agent(a) => CmdError {
                kind: "agent_error",
                code: Some(a.code),
                message: a.message,
            },
            IpcClientError::Io(_) | IpcClientError::Disconnected => {
                CmdError::unavailable(e.to_string())
            }
            other => CmdError {
                kind: "protocol",
                code: None,
                message: other.to_string(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct AgentInfo {
    agent_version: String,
    endpoint: String,
}

/// Devuelve la conexión activa o abre una nueva y empieza a reenviar eventos.
async fn client(app: &AppHandle, conn: &AgentConn) -> Result<Arc<IpcClient>, CmdError> {
    let mut slot = conn.client.lock().await;
    if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
        return Ok(c.clone());
    }
    *slot = None;
    let ep = Endpoint::default_for_user().map_err(|e| CmdError::unavailable(e.to_string()))?;
    let (c, mut events) = IpcClient::connect(&ep, "desktop")
        .await
        .map_err(|e| match e {
            IpcClientError::Io(io) => CmdError::unavailable(format!("Agente no iniciado ({io})")),
            other => other.into(),
        })?;
    let c = Arc::new(c);
    *slot = Some(c.clone());
    let app = app.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            let _ = app.emit("agent-event", ev);
        }
        let _ = app.emit("agent-disconnected", ());
    });
    Ok(c)
}

async fn try_connect(app: &AppHandle, conn: &AgentConn) -> Result<AgentInfo, CmdError> {
    let c = client(app, conn).await?;
    Ok(AgentInfo {
        agent_version: c.agent_version().to_owned(),
        endpoint: Endpoint::default_for_user()
            .map(|e| e.to_string())
            .unwrap_or_default(),
    })
}

#[tauri::command]
async fn agent_connect(app: AppHandle, conn: State<'_, AgentConn>) -> Result<AgentInfo, CmdError> {
    match try_connect(&app, &conn).await {
        Ok(info) => Ok(info),
        Err(first_err) => {
            if find_agent().is_some() {
                spawn_and_connect(&app, &conn).await
            } else {
                Err(first_err)
            }
        }
    }
}

#[tauri::command]
async fn agent_request(
    app: AppHandle,
    conn: State<'_, AgentConn>,
    request: Request,
) -> Result<Response, CmdError> {
    let c = client(&app, &conn).await?;
    Ok(c.request(request).await?)
}

/// Busca `crossscreen-agent` junto al ejecutable de la UI o, en desarrollo,
/// en los directorios de compilación hermanos (`debug`/`release`).
fn find_agent() -> Option<PathBuf> {
    let exe_name = format!("crossscreen-agent{}", std::env::consts::EXE_SUFFIX);
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut candidates = vec![dir.join(&exe_name)];
    if let Some(parent) = dir.parent() {
        for profile in ["debug", "release"] {
            candidates.push(parent.join(profile).join(&exe_name));
        }
        if let Some(root) = parent.parent() {
            for target_dir in ["target", "target-agy", "target-claude"] {
                for profile in ["release", "debug"] {
                    candidates.push(root.join(target_dir).join(profile).join(&exe_name));
                }
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

async fn spawn_and_connect(app: &AppHandle, conn: &AgentConn) -> Result<AgentInfo, CmdError> {
    let path = find_agent().ok_or_else(|| {
        CmdError::unavailable(
            "No se encontró crossscreen-agent junto a la aplicación. Compílalo con `cargo build -p crossscreen-agent` o inícialo a mano.",
        )
    })?;
    let mut cmd = std::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // El agente sigue vivo al cerrar la UI: es un daemon de usuario.
    let mut child = cmd
        .spawn()
        .map_err(|e| CmdError::unavailable(format!("No se pudo lanzar {}: {e}", path.display())))?;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Ok(Some(status)) = child.try_wait() {
            // Código 2 = ya había otro agente: intentar conectar igualmente.
            if status.code() != Some(2) {
                return Err(CmdError::unavailable(format!(
                    "El agente terminó al arrancar ({status}). Revisa agent.log en el directorio de datos."
                )));
            }
        }
        if let Ok(info) = try_connect(app, conn).await {
            return Ok(info);
        }
    }
    Err(CmdError::unavailable(
        "El agente se lanzó pero no respondió a tiempo",
    ))
}

/// Lanza el agente en segundo plano y espera a que acepte conexiones.
#[tauri::command]
async fn start_agent(app: AppHandle, conn: State<'_, AgentConn>) -> Result<AgentInfo, CmdError> {
    spawn_and_connect(&app, &conn).await
}

fn main() {
    tauri::Builder::default()
        .manage(AgentConn::default())
        .invoke_handler(tauri::generate_handler![
            agent_connect,
            agent_request,
            start_agent
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("error al ejecutar la aplicación: {e}");
            std::process::exit(1);
        });
}
