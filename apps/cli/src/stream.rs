//! `crossscreen stream`: emisor del MVP (contrato `docs/09-contrato-sesion-mvp.md`).
//!
//! Envoltura fina sobre `cs_pipeline::run_sender`: resuelve identidad y par,
//! conecta por QUIC con la huella fijada, convierte Ctrl+C (y el cierre de la
//! consola) en una parada ordenada e imprime las métricas al terminar. El
//! monitor virtual lo retira el pipeline en todos los casos.

use crate::bench::PacingArgs;
use anyhow::{bail, Context, Result};
use cs_core::{Mode, StreamMode};
use cs_pipeline::SenderConfig;
use cs_protocol::ALPN;
use cs_security::identity::Identity;
use cs_security::verifier::PeerPolicy;
use cs_security::Fingerprint;
use cs_transport::simloss::LossModel;
use cs_transport::Endpoint;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ModeArg {
    /// Crea un monitor virtual y lo transmite.
    Extend,
    /// Transmite la pantalla principal (sin monitor virtual).
    Duplicate,
}

#[derive(Debug, Clone, clap::Args)]
pub struct StreamArgs {
    /// Dirección del receptor (`crossscreen-viewer --listen`), p. ej. 192.168.1.20:47810.
    pub addr: SocketAddr,
    /// Huella (o prefijo) del receptor emparejado.
    #[arg(long)]
    pub peer: String,
    #[arg(long, value_enum, default_value = "extend")]
    pub mode: ModeArg,
    /// Ancho del monitor virtual. Si no se dan ancho/alto/hz se usa el modo
    /// nativo del receptor (y 1920x1080@60 si el driver no lo admite).
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    #[arg(long)]
    pub hz: Option<u32>,
    /// Tasa de vídeo objetivo (CBR) en Mbit/s.
    #[arg(long, default_value_t = 20.0)]
    pub mbps: f64,
    /// Proporción inicial de paridad FEC para los P-frames (0 = sin FEC).
    #[arg(long, default_value_t = 0.15)]
    pub fec: f32,
    /// Proporción de paridad FEC para los IDR (`max(--keyframe-fec, --fec)`).
    #[arg(long, default_value_t = 0.5)]
    pub keyframe_fec: f32,
    /// Desactivar el entrelazado de shards entre bloques FEC de un mismo frame.
    #[arg(long)]
    pub no_fec_interleave: bool,
    /// Mantener la paridad de los P-frames fija en `--fec` (sin ajuste por `ReceiverStats`).
    #[arg(long)]
    pub no_adaptive_fec: bool,
    /// Valor opcional para `CODECAPI_AVEncVideoGradualIntraRefresh` (experimental).
    #[arg(long)]
    pub intra_refresh: Option<u32>,
    #[command(flatten)]
    pub pacing: PacingArgs,
    /// Terminar tras S segundos de flujo.
    #[arg(long)]
    pub seconds: Option<u64>,
    /// Con el escritorio estático, reenviar la última imagen cada S segundos
    /// (0 = nunca).
    #[arg(long, default_value_t = 1.0)]
    pub static_refresh: f64,
    /// Ventana de gracia en segundos para reconectar tras un corte QUIC
    /// manteniendo vivo el monitor virtual (0 = no reconectar).
    #[arg(long, default_value_t = 15.0)]
    pub reconnect_grace: f64,
    /// Desactivar la inyección de entrada remota (ratón y teclado del receptor).
    #[arg(long)]
    pub no_remote_input: bool,
    /// Permitir que el receptor controle este equipo (ratón y teclado).
    /// Desactivado por defecto: solo se transmite la imagen.
    #[arg(long)]
    pub allow_remote_input: bool,
    /// Sincronizar el portapapeles de texto con el receptor (desactivado por
    /// defecto: puede contener contraseñas u otros datos sensibles).
    #[arg(long)]
    pub clipboard: bool,
    /// Enviar el audio del sistema si el receptor lo pide (desactivado por
    /// defecto: por defecto solo se transmite la imagen).
    #[arg(long)]
    pub audio: bool,
    /// Monitores virtuales a la vez (solo `extend`, máx. 4). El monitor `i`
    /// va al puerto del receptor + `i` (un `crossscreen-viewer --listen` por
    /// puerto). El portapapeles y el audio viajan solo con el primero.
    #[arg(long, default_value_t = 1)]
    pub monitors: u8,
    /// Transferencia de archivos con el receptor (desactivada por defecto):
    /// los que lleguen se guardan en Descargas/CrossScreen.
    #[arg(long)]
    pub files: bool,
    /// Archivo a enviar al receptor al conectar (repetible; implica `--files`).
    #[arg(long = "send-file", value_name = "RUTA")]
    pub send_files: Vec<std::path::PathBuf>,
    /// **Solo pruebas**: simular pérdidas en ráfaga al enviar datagramas,
    /// p. ej. `gilbert:p=0.015,r=0.25,h=0.8` o `bernoulli:p=0.01`.
    #[arg(long)]
    pub sim_loss: Option<LossModel>,
    /// Semilla del inyector `--sim-loss`.
    #[arg(long, default_value_t = 0x5eed)]
    pub sim_loss_seed: u64,
}

pub async fn run(identity: &Identity, name: &str, peer: Fingerprint, a: StreamArgs) -> Result<()> {
    if !(a.mbps > 0.0 && a.mbps <= 500.0) {
        bail!("--mbps fuera de rango (0, 500]");
    }
    if !(0.0..=1.0).contains(&a.fec) {
        bail!("--fec debe estar en [0, 1]");
    }
    if !(0.0..=1.0).contains(&a.keyframe_fec) {
        bail!("--keyframe-fec debe estar en [0, 1]");
    }
    let mut cfg = SenderConfig::new(identity.fingerprint().device_id(), name.to_owned());
    cfg.mode = match a.mode {
        ModeArg::Extend => StreamMode::Extend,
        ModeArg::Duplicate => StreamMode::Duplicate,
    };
    if a.width.is_some() || a.height.is_some() || a.hz.is_some() {
        let m = Mode::new(
            a.width.unwrap_or(1920),
            a.height.unwrap_or(1080),
            a.hz.unwrap_or(60),
        );
        m.validate()
            .map_err(|e| anyhow::anyhow!("modo pedido no válido: {e}"))?;
        cfg.requested_mode = Some(m);
    }
    cfg.bitrate_kbps = (a.mbps * 1000.0).round() as u32;
    cfg.fec_ratio = a.fec;
    cfg.keyframe_fec_ratio = a.keyframe_fec;
    cfg.fec_interleave = !a.no_fec_interleave;
    cfg.adaptive_fec = !a.no_adaptive_fec;
    cfg.intra_refresh = a.intra_refresh;
    cfg.pacing = a.pacing.pacing;
    cfg.pacing_wait = a.pacing.pacing_wait;
    cfg.duration = a.seconds.map(Duration::from_secs);
    cfg.static_refresh =
        (a.static_refresh > 0.0).then(|| Duration::from_secs_f64(a.static_refresh));
    // Por defecto solo se transmite la imagen: el control remoto se pide.
    cfg.accept_input = a.allow_remote_input && !a.no_remote_input;
    cfg.clipboard = a.clipboard;
    cfg.audio = a.audio;
    let mut files_tx = None;
    if a.files || !a.send_files.is_empty() {
        let inbox = cs_files::inbox_dir().context("no se encontró la carpeta de Descargas")?;
        let (handle, tx, mut events) = cs_files::FilesHandle::new(inbox.clone());
        cs_log::out!(
            "Transferencia de archivos activada (recibidos en {})",
            inbox.display()
        );
        for f in &a.send_files {
            if !f.is_file() {
                bail!("--send-file: {} no es un archivo", f.display());
            }
            let _ = tx.send(f.clone());
        }
        // La cola sigue abierta mientras dure la sesión.
        files_tx = Some(tx);
        cfg.files = Some(handle);
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    cs_files::FileEvent::Sent { name, size } => {
                        cs_log::out!("Archivo enviado: {name} ({size} bytes)")
                    }
                    cs_files::FileEvent::Received { path, size, .. } => {
                        cs_log::out!("Archivo recibido: {} ({size} bytes)", path.display())
                    }
                    cs_files::FileEvent::Failed {
                        name,
                        incoming,
                        error,
                    } => cs_log::err_out!(
                        "Archivo {} {name}: {error}",
                        if incoming {
                            "no recibido"
                        } else {
                            "no enviado"
                        }
                    ),
                }
            }
        });
    }
    cfg.sim_loss = a.sim_loss.map(|m| (m, a.sim_loss_seed));

    // Señal de parada: Ctrl+C, Ctrl+Break y cierre de la consola.
    #[cfg(windows)]
    cs_pipeline::win::console::enable_ctrl_c();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_stop_signal().await;
        cs_log::err_out!("Parando… (retirando el monitor virtual)");
        let _ = stop_tx.send(true);
    });

    let monitors =
        cs_pipeline::multi::check_monitors(a.monitors, cfg.mode).map_err(|e| anyhow::anyhow!(e))?;
    let bind: SocketAddr = if a.addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()?;
    let endpoint = Endpoint::client(bind, identity.clone())?;
    cfg.reconnect = (a.reconnect_grace > 0.0).then(|| cs_pipeline::ReconnectConfig {
        endpoint: endpoint.clone(),
        addr: a.addr,
        peer,
        grace: Duration::from_secs_f64(a.reconnect_grace),
    });
    // Linux: diálogo del sistema para elegir la pantalla, antes de conectar
    // (así no consume los temporizadores del receptor).
    #[cfg(not(windows))]
    if cfg.mode == StreamMode::Duplicate {
        cs_log::out!("Elige la pantalla en el diálogo del sistema…");
    }
    // Un monitor = una sesión independiente al puerto base + i (ADR-0009).
    let mut sessions = Vec::with_capacity(usize::from(monitors));
    for i in 0..monitors {
        let addr = cs_pipeline::multi::monitor_addr(a.addr, i)
            .context("el puerto del receptor es demasiado alto para tantos monitores")?;
        let cfg_i = cs_pipeline::multi::monitor_config(&cfg, i);
        let source = cs_pipeline::prepare_source(&cfg_i).await?;
        let conn = endpoint
            .connect(addr, PeerPolicy::Exactly(peer), ALPN)
            .await
            .with_context(|| {
                format!(
                    "no se pudo conectar con el receptor {addr}. Comprueba que esté escuchando (crossscreen-viewer --listen, o «Recibir pantalla» en el panel) y que su firewall permita crossscreen-viewer (en Windows: «Permitir» en redes privadas)"
                )
            })?;
        cs_log::out!(
            "{}Conectado a {} ({}) — RTT {:?}, datagrama máx. {:?} B",
            if monitors > 1 {
                format!("Monitor {}: ", i + 1)
            } else {
                String::new()
            },
            conn.remote_addr(),
            conn.peer_fingerprint().short(),
            conn.rtt(),
            conn.max_datagram_size()
        );
        sessions.push((cfg_i, source, conn));
    }
    let tasks: Vec<_> = sessions
        .into_iter()
        .map(|(cfg_i, source, conn)| {
            tokio::spawn(cs_pipeline::run_sender(
                conn,
                cfg_i,
                source,
                stop_rx.clone(),
            ))
        })
        .collect();
    let mut results = Vec::with_capacity(tasks.len());
    for t in tasks {
        results.push(t.await.context("tarea del monitor")?);
    }
    drop(files_tx);
    endpoint.close();
    let mut fatal = None;
    for (i, res) in results.into_iter().enumerate() {
        if monitors > 1 {
            cs_log::out!("— Monitor {} —", i + 1);
        }
        let report = res?;
        cs_log::out!("{report}");
        if let Some(e) = &report.worker.fatal {
            fatal.get_or_insert_with(|| e.clone());
        }
    }
    if let Some(e) = fatal {
        bail!("la sesión terminó por un fallo de captura/codificación: {e}");
    }
    Ok(())
}

async fn wait_for_stop_signal() {
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        match (
            windows::ctrl_c(),
            windows::ctrl_close(),
            windows::ctrl_break(),
        ) {
            (Ok(mut c), Ok(mut close), Ok(mut brk)) => {
                tokio::select! {
                    _ = c.recv() => {}
                    _ = close.recv() => {}
                    _ = brk.recv() => {}
                }
            }
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
