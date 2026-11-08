//! `crossscreen-viewer`: proceso receptor nativo de CrossScreen.
//!
//! Dos modos:
//! - `--file x.h264`: reproduce un fichero H.264 Annex-B a su frecuencia
//!   (PoC-5): lo parte en *access units*, las decodifica por hardware
//!   (`cs-decode`) y las presenta con D3D11 + DXGI flip (`cs-render`).
//! - `--listen ip:puerto`: receptor del MVP F2 según
//!   `docs/09-contrato-sesion-mvp.md`. Escucha QUIC (solo pares
//!   emparejados), sigue la secuencia de control, reensambla los datagramas
//!   de vídeo, decodifica por hardware y presenta sin copias, con el cursor
//!   fuera de banda.
//!
//! ```text
//! crossscreen-viewer --file x.h264 --fps 60 [--fullscreen] [--monitor N] [--seconds S]
//! crossscreen-viewer --listen 0.0.0.0:47810 [--data-dir D] [--fullscreen] [--monitor N]
//!                    [--size 1920x1080] [--once] [--seconds S] [--same-host-latency]
//!                    [--screenshot p.png --screenshot-frame N [--screenshot-concealed]]
//!                    [--freeze-on-loss] [--keyframe-policy calm|legacy]
//! ```

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod clock;
#[cfg(windows)]
mod decode_worker;
#[cfg(windows)]
mod listen;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod session;

#[cfg(windows)]
use clap::Parser;
#[cfg(windows)]
use std::net::SocketAddr;
#[cfg(windows)]
use std::path::PathBuf;

/// Visor de CrossScreen: reproduce un fichero o recibe un flujo en vivo.
#[cfg(windows)]
#[derive(Debug, Parser)]
#[command(
    name = "crossscreen-viewer",
    version,
    group(clap::ArgGroup::new("source").required(true).args(["file", "listen"]))
)]
pub struct Args {
    /// Fichero H.264 Annex-B a reproducir (modo PoC-5).
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Escuchar sesiones de streaming en esta dirección UDP (p. ej. `0.0.0.0:47810`).
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    /// Directorio de datos (identidad y pares de confianza), el mismo que usa
    /// `crossscreen --data-dir`.
    #[arg(long, env = "CROSSSCREEN_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Salir al terminar la primera sesión en vez de volver a escuchar.
    #[arg(long)]
    pub once: bool,
    /// Fuente y receptor en el mismo equipo (mismo reloj QPC): informar de la
    /// latencia captura → presentación. No usar entre equipos distintos.
    #[arg(long)]
    pub same_host_latency: bool,
    /// Frecuencia de reproducción (solo `--file`).
    #[arg(long, default_value_t = 60.0)]
    pub fps: f64,
    /// Ventana sin bordes a pantalla completa.
    #[arg(long)]
    pub fullscreen: bool,
    /// Índice del monitor (orden de enumeración de winit; se listan al arrancar).
    #[arg(long)]
    pub monitor: Option<usize>,
    /// Cerrar tras S segundos. En `--file` el fichero se repite en bucle
    /// hasta entonces (sin este parámetro se reproduce una vez).
    #[arg(long)]
    pub seconds: Option<f64>,
    /// Tamaño del área cliente en modo ventana, `ANCHOxALTO`. Por defecto
    /// 1280x720 con `--file` y 1920x1080 con `--listen` (es el `native_mode`
    /// que se anuncia en ventana).
    #[arg(long)]
    pub size: Option<String>,
    /// Desactiva vsync (`Present(0, ALLOW_TEARING)`).
    #[arg(long)]
    pub no_vsync: bool,
    /// No esperar a la GPU tras cada frame (solo `--file`; sin medición de tiempo GPU).
    #[arg(long)]
    pub no_gpu_wait: bool,
    /// Guarda en PNG el frame presentado número N (ver `--screenshot-frame`).
    #[arg(long)]
    pub screenshot: Option<PathBuf>,
    /// Frame de vídeo presentado (desde 0) que se captura con `--screenshot`.
    #[arg(long, default_value_t = 30)]
    pub screenshot_frame: u64,
    /// Con `--screenshot`: capturar el primer frame presentado con posibles
    /// artefactos de ocultación (decodificado tras una pérdida) a partir del
    /// número `--screenshot-frame`.
    #[arg(long)]
    pub screenshot_concealed: bool,
    /// Comportamiento anterior ante pérdidas: descartar los P-frames y
    /// congelar la imagen hasta el siguiente IDR (por defecto se siguen
    /// decodificando y el MFT oculta las referencias que faltan).
    #[arg(long)]
    pub freeze_on_loss: bool,
    /// Política de `RequestKeyframe`: `calm` (una por episodio, espera
    /// `max(3·RTT, 250 ms)` con *backoff* hasta 2 s) o `legacy` (cada 100 ms
    /// mientras dure la pérdida).
    #[arg(long, value_enum, default_value_t = session::KeyframePolicy::Calm)]
    pub keyframe_policy: session::KeyframePolicy,
    /// Ocultar el cursor superpuesto de demostración (solo `--file`).
    #[arg(long)]
    pub no_cursor: bool,
    /// Desactivar el envío de entrada remota (ratón y teclado locales hacia la fuente).
    #[arg(long)]
    pub no_remote_input: bool,
    /// Desactivar la sincronización del portapapeles de texto con la fuente.
    #[arg(long)]
    pub no_clipboard: bool,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    if args.listen.is_some() {
        listen::main(args)
    } else {
        app::main(args)
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("crossscreen-viewer: por ahora solo hay backend para Windows (Linux: VAAPI + DMA-BUF pendiente)");
    std::process::exit(2);
}

/// `ANCHOxALTO` → `(ancho, alto)`.
#[cfg(windows)]
pub fn parse_size(s: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("--size debe ser ANCHOxALTO"))?;
    Ok((w.parse()?, h.parse()?))
}

/// Guarda RGBA8 como PNG (verificación con `--screenshot`).
#[cfg(windows)]
pub fn save_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}
