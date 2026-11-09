//! `crossscreen`: CLI de diagnóstico.
//!
//! Subcomandos reales (sin simulaciones):
//! - `id`, `peers`, `forget`: identidad y almacén de confianza.
//! - `pair-listen` / `pair`: emparejamiento con PIN (SPAKE2 sobre QUIC).
//! - `discover`: lista equipos CrossScreen en la LAN por mDNS.
//! - `bench-serve` / `bench`: banco de pruebas del transporte de vídeo con
//!   frames sintéticos (PoC-4). No captura ni codifica pantalla.
//! - `stream`: emisor del MVP (monitor virtual → captura → H.264 → QUIC),
//!   según `docs/09-contrato-sesion-mvp.md`.

mod bench;
mod display;
mod stream;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cs_core::Roles;
use cs_discovery::{Announcement, Discovery, DiscoveryEvent};
use cs_protocol::{ALPN_PAIRING, PROTOCOL_VERSION};
use cs_security::identity::Identity;
use cs_security::pairing::Pin;
use cs_security::ratelimit::AttemptLimiter;
use cs_security::trust::TrustStore;
use cs_security::verifier::PeerPolicy;
use cs_security::Fingerprint;
use cs_transport::pairing::{pair_as_initiator, pair_as_responder};
use cs_transport::Endpoint;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "crossscreen",
    version,
    about = "CrossScreen: herramientas de diagnóstico"
)]
struct Cli {
    /// Directorio de datos (identidad y pares de confianza).
    #[arg(long, global = true, env = "CROSSSCREEN_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Nombre visible de este equipo.
    #[arg(long, global = true)]
    name: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Muestra (y crea si hace falta) la identidad de este equipo.
    Id,
    /// Lista los equipos emparejados.
    Peers,
    /// Olvida un equipo emparejado (prefijo hexadecimal de su huella).
    Forget { fingerprint_prefix: String },
    /// Muestra un PIN y espera a que otro equipo se empareje.
    PairListen {
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: SocketAddr,
        /// No anunciar por mDNS.
        #[arg(long)]
        no_mdns: bool,
    },
    /// Se empareja con un equipo que está en `pair-listen`.
    Pair { addr: SocketAddr, pin: String },
    /// Lista equipos CrossScreen en la red local.
    Discover {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Monitores virtuales: listar, mostrar, retirar, medir ciclos.
    #[command(subcommand)]
    Display(display::DisplayCmd),
    /// Receptor del banco de pruebas de transporte.
    BenchServe {
        #[arg(long, default_value = "0.0.0.0:47800")]
        bind: SocketAddr,
    },
    /// Emisor del banco de pruebas: envía frames sintéticos al receptor.
    Bench {
        addr: SocketAddr,
        /// Huella (o prefijo) del receptor emparejado.
        #[arg(long)]
        peer: String,
        #[arg(long, default_value_t = 20.0)]
        mbps: f64,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Proporción de paridad FEC (0 = sin FEC).
        #[arg(long, default_value_t = 0.10)]
        fec: f32,
        #[command(flatten)]
        pacing: bench::PacingArgs,
    },
    /// Emisor del MVP: transmite un monitor virtual (o la pantalla
    /// principal) a un `crossscreen-viewer --listen` emparejado.
    Stream(stream::StreamArgs),
}

struct Ctx {
    dir: PathBuf,
    name: String,
}

impl Ctx {
    fn identity(&self) -> Result<Identity> {
        Identity::load_or_create(&self.dir).context("cargando identidad")
    }
    fn trust(&self) -> Result<TrustStore> {
        TrustStore::open(self.dir.join("trusted-peers.json"))
            .context("abriendo almacén de confianza")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(path) = cs_log::init("cli", "warn") {
        tracing::debug!("registro: {}", path.display());
    }
    cs_log::finish(run().await)
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let dir = match cli.data_dir {
        Some(d) => d,
        None => directories::ProjectDirs::from("org", "CrossScreen", "CrossScreen")
            .context("no se pudo determinar el directorio de datos")?
            .data_dir()
            .to_path_buf(),
    };
    let name = cli.name.unwrap_or_else(default_name);
    let ctx = Ctx { dir, name };

    match cli.cmd {
        Cmd::Id => cmd_id(&ctx),
        Cmd::Display(c) => display::run(c),
        Cmd::Peers => cmd_peers(&ctx),
        Cmd::Forget { fingerprint_prefix } => cmd_forget(&ctx, &fingerprint_prefix),
        Cmd::PairListen { bind, no_mdns } => cmd_pair_listen(&ctx, bind, !no_mdns).await,
        Cmd::Pair { addr, pin } => cmd_pair(&ctx, addr, &pin).await,
        Cmd::Discover { seconds } => cmd_discover(&ctx, seconds).await,
        Cmd::Stream(args) => {
            let peer = resolve_peer(&ctx.trust()?, &args.peer)?;
            stream::run(&ctx.identity()?, &ctx.name, peer, args).await
        }
        Cmd::BenchServe { bind } => bench::serve(&ctx.identity()?, &ctx.trust()?, bind).await,
        Cmd::Bench {
            addr,
            peer,
            mbps,
            fps,
            seconds,
            fec,
            pacing,
        } => {
            let peer = resolve_peer(&ctx.trust()?, &peer)?;
            bench::run(
                &ctx.identity()?,
                addr,
                peer,
                bench::Params {
                    mbps,
                    fps,
                    seconds,
                    fec,
                    pacing,
                },
            )
            .await
        }
    }
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "CrossScreen".into())
}

fn cmd_id(ctx: &Ctx) -> Result<()> {
    let id = ctx.identity()?;
    let fp = id.fingerprint();
    cs_log::out!("Nombre:      {}", ctx.name);
    cs_log::out!("Device ID:   {}", fp.device_id());
    cs_log::out!("Huella:      {fp}");
    cs_log::out!("Huella corta {}", fp.short());
    cs_log::out!("Datos en:    {}", ctx.dir.display());
    Ok(())
}

fn cmd_peers(ctx: &Ctx) -> Result<()> {
    let trust = ctx.trust()?;
    let mut any = false;
    for p in trust.peers() {
        any = true;
        cs_log::out!(
            "{}  {}  (emparejado: {})",
            p.fingerprint.short(),
            p.name,
            p.paired_at
        );
    }
    if !any {
        cs_log::out!("No hay equipos emparejados.");
    }
    Ok(())
}

fn resolve_peer(trust: &TrustStore, prefix: &str) -> Result<Fingerprint> {
    let prefix = prefix.replace('-', "").to_lowercase();
    let matches: Vec<Fingerprint> = trust
        .fingerprints()
        .into_iter()
        .filter(|fp| fp.to_string().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!("ningún equipo emparejado coincide con '{prefix}' (usa `crossscreen peers`)"),
        _ => bail!("el prefijo '{prefix}' es ambiguo"),
    }
}

fn cmd_forget(ctx: &Ctx, prefix: &str) -> Result<()> {
    let mut trust = ctx.trust()?;
    let fp = resolve_peer(&trust, prefix)?;
    trust.remove(&fp)?;
    cs_log::out!("Olvidado {}", fp.short());
    Ok(())
}

async fn cmd_pair_listen(ctx: &Ctx, bind: SocketAddr, mdns: bool) -> Result<()> {
    let identity = ctx.identity()?;
    let mut trust = ctx.trust()?;
    let endpoint = Endpoint::server(
        bind,
        identity.clone(),
        PeerPolicy::AnyForPairing,
        ALPN_PAIRING,
    )?;
    let port = endpoint.local_addr()?.port();

    let _discovery = if mdns {
        let mut d = Discovery::new()?;
        d.announce(&Announcement {
            device_id: identity.fingerprint().device_id(),
            name: ctx.name.clone(),
            roles: Roles::BOTH,
            protocol_major: PROTOCOL_VERSION.major,
            session_port: 0,
            pairing_port: Some(port),
        })?;
        Some(d)
    } else {
        None
    };

    let pin = Pin::generate();
    cs_log::out!("Escuchando emparejamiento en el puerto UDP {port}");
    println!();
    // El PIN solo va a la consola, nunca al registro (los registros se comparten).
    println!("    PIN:  {} {}", &pin.as_str()[..3], &pin.as_str()[3..]);
    println!();
    println!(
        "En el otro equipo: crossscreen pair <esta-ip>:{port} {}",
        pin.as_str()
    );
    tracing::info!("PIN de emparejamiento mostrado en consola (no se registra)");
    cs_log::out!("Mi huella: {}", identity.fingerprint().short());

    let mut limiter = AttemptLimiter::default();
    let peer = tokio::time::timeout(
        Duration::from_secs(120),
        pair_as_responder(&endpoint, &pin, &ctx.name, &mut limiter),
    )
    .await
    .context("el PIN caducó (2 minutos)")??;
    trust.add(peer.fingerprint, &peer.name)?;
    cs_log::out!(
        "Emparejado con '{}' ({})",
        peer.name,
        peer.fingerprint.short()
    );
    endpoint.close();
    Ok(())
}

async fn cmd_pair(ctx: &Ctx, addr: SocketAddr, pin: &str) -> Result<()> {
    let pin = Pin::parse(pin)?;
    let identity = ctx.identity()?;
    let mut trust = ctx.trust()?;
    let bind: SocketAddr = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()?;
    let endpoint = Endpoint::client(bind, identity)?;
    let peer = pair_as_initiator(&endpoint, addr, &pin, &ctx.name).await?;
    trust.add(peer.fingerprint, &peer.name)?;
    cs_log::out!(
        "Emparejado con '{}' ({})",
        peer.name,
        peer.fingerprint.short()
    );
    cs_log::out!("Compara esta huella con la que muestra el otro equipo.");
    Ok(())
}

async fn cmd_discover(ctx: &Ctx, seconds: u64) -> Result<()> {
    let own = ctx.identity()?.fingerprint().device_id();
    let trust = ctx.trust()?;
    let discovery = Discovery::new()?;
    let mut browser = discovery.browse(Some(own))?;
    cs_log::out!("Buscando equipos durante {seconds} s…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut found = 0;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, browser.next()).await {
        if let DiscoveryEvent::Found(p) = ev {
            found += 1;
            let paired = trust
                .fingerprints()
                .iter()
                .any(|fp| fp.device_id() == p.device_id);
            cs_log::out!(
                "- {} [{}] v{} roles={} sesión={} emparejamiento={} {:?}{}",
                p.name,
                p.device_id,
                p.protocol_major,
                p.roles.to_txt(),
                p.session_port,
                p.pairing_port.map_or("-".into(), |x| x.to_string()),
                p.addresses,
                if paired { "  (emparejado)" } else { "" }
            );
        }
    }
    if found == 0 {
        cs_log::out!("No se encontró ningún equipo (¿multicast bloqueado por firewall o aislamiento del AP?).");
    }
    Ok(())
}
