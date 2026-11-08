//! `crossscreen-agent --probe`: cliente de diagnóstico. Se conecta al agente
//! en ejecución y ejecuta solo peticiones de lectura (no cambia nada).

use anyhow::{Context, Result};
use cs_ipc::{Endpoint, IpcClient, Request};

pub async fn run() -> Result<i32> {
    let ep = Endpoint::default_for_user().context("determinando el punto IPC")?;
    let (client, _events) = match IpcClient::connect(&ep, "probe").await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("No se pudo conectar con el agente en {ep}: {e}");
            return Ok(3);
        }
    };
    println!("Conectado al agente v{} en {ep}", client.agent_version());
    let mut failures = 0;
    for req in [
        Request::GetStatus,
        Request::GetIdentity,
        Request::ListPeers,
        Request::ListDisplays,
    ] {
        let label = format!("{req:?}");
        match client.request(req).await {
            Ok(resp) => println!(
                "{label}: {}",
                serde_json::to_string_pretty(&resp).unwrap_or_else(|e| e.to_string())
            ),
            Err(e) => {
                failures += 1;
                println!("{label}: ERROR {e}");
            }
        }
    }
    Ok(if failures == 0 { 0 } else { 4 })
}
