//! Registros a archivo para todos los programas de CrossScreen.
//!
//! Cada ejecución escribe su propio archivo en [`log_dir`]:
//! `<componente>-<AAAAMMDD-HHMMSS>-<pid>.log` (hora UTC). Se conservan los
//! últimos [`KEEP_PER_COMPONENT`] de cada componente. El archivo recibe al
//! menos el nivel `info` aunque la consola muestre menos, y también los
//! pánicos (con archivo y línea) y lo impreso con [`out!`] / [`err_out!`].
//!
//! Ubicación:
//! - Windows: `%LOCALAPPDATA%\CrossScreen\logs`
//! - Linux: `$XDG_STATE_HOME/crossscreen/logs` (por defecto
//!   `~/.local/state/crossscreen/logs`)
//! - `CROSSSCREEN_LOG_DIR` la sustituye en cualquier sistema.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[doc(hidden)]
pub use tracing;

/// Archivos que se conservan por componente.
pub const KEEP_PER_COMPONENT: usize = 10;
/// Variable de entorno que fija otra carpeta de registros.
pub const LOG_DIR_ENV: &str = "CROSSSCREEN_LOG_DIR";

/// Imprime en la salida estándar **y** lo guarda en el registro.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        println!("{s}");
        $crate::tracing::info!(target: "salida", "{s}");
    }};
}

/// Imprime en la salida de errores **y** lo guarda en el registro.
#[macro_export]
macro_rules! err_out {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        eprintln!("{s}");
        $crate::tracing::warn!(target: "salida", "{s}");
    }};
}

/// Carpeta de registros de este usuario (no se crea aquí).
pub fn log_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LOG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("CrossScreen").join("logs"))
    }
    #[cfg(not(windows))]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(state.join("crossscreen").join("logs"))
    }
}

/// Configura `tracing` con salida a consola (filtro `RUST_LOG` o
/// `console_default`) y a un archivo nuevo en [`log_dir`] (filtro `RUST_LOG`
/// o `info`), e instala un *hook* de pánico que lo registra.
///
/// Devuelve la ruta del archivo; `None` si no se pudo crear (la consola sigue
/// funcionando). Llamar una sola vez, al principio de `main`.
pub fn init(component: &str, console_default: &str) -> Option<PathBuf> {
    init_with(component, console_default, None)
}

/// Como [`init`], pero sin capa de consola (procesos sin terminal, como el
/// agente lanzado por el panel).
pub fn init_file_only(component: &str, file_filter: &str) -> Option<PathBuf> {
    init_with(component, "off", Some(file_filter))
}

fn env_or(default: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
}

/// Añade directivas fijas a un filtro (son válidas por construcción).
fn with_directives(mut filter: EnvFilter, directives: &[&str]) -> EnvFilter {
    for d in directives {
        if let Ok(d) = d.parse() {
            filter = filter.add_directive(d);
        }
    }
    filter
}

/// Lo que va a la consola por `println!` ([`out!`]) no se repite como traza,
/// y la cabecera de inicio solo interesa en el archivo.
const CONSOLE_DIRECTIVES: &[&str] = &["salida=off", "inicio=off"];
/// El archivo guarda siempre la salida, la cabecera, el error final y los
/// pánicos, aunque `RUST_LOG` baje el nivel general.
const FILE_DIRECTIVES: &[&str] = &["salida=info", "inicio=info", "fin=info", "panico=error"];

/// `file_only`: sin consola y con este filtro para el archivo.
fn init_with(component: &str, console_default: &str, file_only: Option<&str>) -> Option<PathBuf> {
    let console = file_only.is_none();
    let file_filter = match file_only {
        Some(f) => EnvFilter::try_new(f).unwrap_or_else(|_| EnvFilter::new("info")),
        None => env_or("info"),
    };
    let file_filter = with_directives(file_filter, FILE_DIRECTIVES);
    let file = log_dir().and_then(|dir| open_log_file(&dir, component).ok());
    let path = file.as_ref().map(|(p, _)| p.clone());

    let console_layer = console.then(|| {
        tracing_subscriber::fmt::layer()
            .with_filter(with_directives(env_or(console_default), CONSOLE_DIRECTIVES))
    });
    let file_layer = file.map(|(_, f)| {
        let f = Arc::new(Mutex::new(f));
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(move || SharedFile(Arc::clone(&f)))
            .with_filter(file_filter)
    });
    let _ = tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init();

    install_panic_hook();
    tracing::info!(
        target: "inicio",
        "{component} {} (build {}) en {} {}; args: {:?}",
        env!("CARGO_PKG_VERSION"),
        option_env!("CROSSSCREEN_BUILD_ID").unwrap_or("dev"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        redact_args(std::env::args()),
    );
    path
}

/// Argumentos para el registro, con los PIN de emparejamiento (6 dígitos,
/// p. ej. `crossscreen pair <ip> 123456`) ocultos: los registros se comparten.
fn redact_args(args: impl Iterator<Item = String>) -> Vec<String> {
    args.map(|a| {
        if a.len() == 6 && a.bytes().all(|b| b.is_ascii_digit()) {
            "******".to_owned()
        } else {
            a
        }
    })
    .collect()
}

/// Registra el error con el que termina `main` (si lo hay) y lo devuelve.
pub fn finish<T, E: std::fmt::Debug>(result: Result<T, E>) -> Result<T, E> {
    if let Err(e) = &result {
        tracing::error!(target: "fin", "terminó con error: {e:?}");
    }
    result
}

/// Escritor que comparte el archivo entre hilos.
struct SharedFile(Arc<Mutex<File>>);

impl Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Escritura completa y sin búfer: si el proceso muere, lo escrito queda.
        f.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(sin mensaje)".into());
        tracing::error!(
            target: "panico",
            "PÁNICO en el hilo {:?} ({location}): {payload}\n{}",
            thread.name().unwrap_or("?"),
            std::backtrace::Backtrace::force_capture()
        );
        previous(info);
    }));
}

fn open_log_file(dir: &Path, component: &str) -> std::io::Result<(PathBuf, File)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // Por si ya existía con permisos más abiertos.
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    prune(dir, component, KEEP_PER_COMPONENT.saturating_sub(1));
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(file_name(component, secs, std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&path)?;
    Ok((path, file))
}

/// `componente-AAAAMMDD-HHMMSS-pid.log` (UTC).
fn file_name(component: &str, unix_secs: u64, pid: u32) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    let t = unix_secs % 86_400;
    format!(
        "{component}-{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{pid}.log",
        t / 3600,
        (t / 60) % 60,
        t % 60
    )
}

/// Borra los registros más antiguos de `component` dejando `keep`.
fn prune(dir: &Path, component: &str, keep: usize) {
    let prefix = format!("{component}-");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".log"))
        })
        .collect();
    // El nombre empieza por la fecha: el orden alfabético es el cronológico.
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for old in files.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}

/// Días desde 1970-01-01 → (año, mes, día) del calendario gregoriano
/// (algoritmo de Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, file_name, prune, redact_args};

    #[test]
    fn pin_is_redacted_from_args() {
        let args = [
            "crossscreen",
            "pair",
            "192.168.1.5:47901",
            "633192",
            "--seconds",
            "60",
        ];
        assert_eq!(
            redact_args(args.iter().map(|s| s.to_string())),
            [
                "crossscreen",
                "pair",
                "192.168.1.5:47901",
                "******",
                "--seconds",
                "60"
            ]
        );
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(
            file_name("viewer", 1_794_182_400 + 3_723, 42),
            "viewer-20261109-010203-42.log"
        );
    }

    #[test]
    fn prune_keeps_newest_of_component() {
        let dir = std::env::temp_dir().join(format!("cs-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "viewer-20260101-000000-1.log",
            "viewer-20260102-000000-1.log",
            "viewer-20260103-000000-1.log",
            "agent-20250101-000000-1.log",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        prune(&dir, "viewer", 2);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "agent-20250101-000000-1.log",
                "viewer-20260102-000000-1.log",
                "viewer-20260103-000000-1.log"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
