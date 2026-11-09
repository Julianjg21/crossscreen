//! Transferencia de archivos de CrossScreen (ADR-0010).
//!
//! - Cada archivo viaja en su propio stream QUIC unidireccional con la
//!   cabecera de [`cs_protocol::files`].
//! - Al enviar se limita la velocidad ([`RATE_BYTES_PER_SEC`]) para no ahogar
//!   el vídeo, que va por datagramas en la misma conexión.
//! - Al recibir:
//!   - el nombre se sanea;
//!   - se escribe en un `.part` y, completo y con el tamaño anunciado, se
//!     renombra sin sobrescribir nada (`foto (1).png`);
//!   - en Windows se marca como descargado de Internet (*Mark of the Web*),
//!     para que el sistema avise antes de ejecutarlo.
//! - [`FileService`] une las dos cosas sobre una conexión: acepta los
//!   archivos entrantes y envía, en orden, los que se pongan en la cola de
//!   [`FilesHandle`]. La cola sobrevive a las reconexiones.

use cs_protocol::files::{sanitize_name, FileHeader};
use cs_transport::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

/// Velocidad máxima de envío: 6 MB/s (~48 Mbit/s). Deja sitio al vídeo
/// (20 Mbit/s por defecto) incluso en Wi-Fi.
pub const RATE_BYTES_PER_SEC: u64 = 6_000_000;
/// Variable de entorno para cambiar la carpeta de recibidos.
pub const FILES_DIR_ENV: &str = "CROSSSCREEN_FILES_DIR";
/// Tiempo máximo para recibir la cabecera de un archivo.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK: usize = 64 * 1024;
/// Al empezar la sesión el aviso `FilesAvailable` del par puede llegar justo
/// después de encolar un archivo: se espera este tiempo antes de fallar.
const PEER_WAIT: Duration = Duration::from_secs(5);

/// Carpeta donde se guardan los archivos recibidos: `Descargas/CrossScreen`
/// (o [`FILES_DIR_ENV`]).
pub fn inbox_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os(FILES_DIR_ENV) {
        return Some(PathBuf::from(d));
    }
    downloads_dir().map(|d| d.join("CrossScreen"))
}

#[cfg(windows)]
fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads"))
}

#[cfg(not(windows))]
fn downloads_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let from_user_dirs = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|s| xdg_download_dir(&s, &home));
    Some(from_user_dirs.unwrap_or_else(|| home.join("Downloads")))
}

/// Interpreta `XDG_DOWNLOAD_DIR="$HOME/Descargas"` de `user-dirs.dirs`.
#[cfg_attr(windows, allow(dead_code))]
fn xdg_download_dir(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    let line = user_dirs
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("XDG_DOWNLOAD_DIR="))?;
    let v = line["XDG_DOWNLOAD_DIR=".len()..].trim_matches('"');
    let p = match v.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(v),
    };
    (p.is_absolute() && p != home).then_some(p)
}

/// Ruta libre para `name` en `dir`: `name`, `stem (1).ext`, `stem (2).ext`…
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("rango infinito")
}

/// Lo que va pasando con las transferencias (para la UI y el registro).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// Archivo enviado entero al par.
    Sent { name: String, size: u64 },
    /// Archivo recibido y guardado.
    Received {
        name: String,
        path: PathBuf,
        size: u64,
    },
    /// Fallo al enviar (`incoming = false`) o recibir (`true`).
    Failed {
        name: String,
        incoming: bool,
        error: String,
    },
}

/// Cola de salida y destino de eventos; se clona en cada sesión (y en cada
/// reconexión) sin perder lo que esté en cola.
#[derive(Clone)]
pub struct FilesHandle {
    outbox: Arc<Mutex<mpsc::UnboundedReceiver<PathBuf>>>,
    events: mpsc::UnboundedSender<FileEvent>,
    inbox: PathBuf,
}

impl std::fmt::Debug for FilesHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesHandle")
            .field("inbox", &self.inbox)
            .finish_non_exhaustive()
    }
}

impl FilesHandle {
    /// Crea la cola. Devuelve el handle, el extremo para encolar archivos a
    /// enviar y el de eventos.
    pub fn new(
        inbox: PathBuf,
    ) -> (
        Self,
        mpsc::UnboundedSender<PathBuf>,
        mpsc::UnboundedReceiver<FileEvent>,
    ) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        (
            Self {
                outbox: Arc::new(Mutex::new(out_rx)),
                events: ev_tx,
                inbox,
            },
            out_tx,
            ev_rx,
        )
    }

    pub fn inbox(&self) -> &Path {
        &self.inbox
    }
}

/// Servicio de archivos sobre una conexión. Al soltarlo se paran sus tareas
/// (las transferencias en curso se cortan; la cola se conserva).
#[derive(Debug)]
pub struct FileService {
    peer_enabled: tokio::sync::watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl FileService {
    pub fn start(conn: Connection, handle: &FilesHandle) -> Self {
        let (peer_enabled, peer_rx) = tokio::sync::watch::channel(false);
        let mut tasks = Vec::new();
        // Entrantes: cada stream en su tarea.
        {
            let conn = conn.clone();
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(recv) = conn.accept_uni().await {
                    let h = h.clone();
                    tokio::spawn(async move {
                        let ev = match receive_file(recv, &h.inbox).await {
                            Ok((name, path, size)) => {
                                tracing::info!(%name, path = %path.display(), size, "archivo recibido");
                                FileEvent::Received { name, path, size }
                            }
                            Err((name, error)) => {
                                tracing::warn!(%name, %error, "archivo no recibido");
                                FileEvent::Failed {
                                    name,
                                    incoming: true,
                                    error,
                                }
                            }
                        };
                        let _ = h.events.send(ev);
                    });
                }
            }));
        }
        // Salientes: en orden, uno detrás de otro.
        {
            let h = handle.clone();
            let mut enabled = peer_rx;
            tasks.push(tokio::spawn(async move {
                let mut outbox = h.outbox.lock().await;
                while let Some(path) = outbox.recv().await {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let ready = *enabled.borrow()
                        || matches!(
                            tokio::time::timeout(PEER_WAIT, enabled.wait_for(|e| *e)).await,
                            Ok(Ok(_))
                        );
                    let res = if ready {
                        send_file(&conn, &path).await
                    } else {
                        Err("el otro equipo no tiene activada la transferencia de archivos".into())
                    };
                    let ev = match res {
                        Ok(size) => {
                            tracing::info!(%name, size, "archivo enviado");
                            FileEvent::Sent { name, size }
                        }
                        Err(error) => {
                            tracing::warn!(%name, %error, "archivo no enviado");
                            FileEvent::Failed {
                                name,
                                incoming: false,
                                error,
                            }
                        }
                    };
                    let _ = h.events.send(ev);
                }
            }));
        }
        Self {
            peer_enabled,
            tasks,
        }
    }

    /// El par anunció (o retiró) que acepta archivos.
    pub fn set_peer_enabled(&self, enabled: bool) {
        self.peer_enabled.send_replace(enabled);
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Envía un archivo por un stream nuevo. Devuelve los bytes enviados.
pub async fn send_file(conn: &Connection, path: &Path) -> Result<u64, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("no se puede leer: {e}"))?;
    if !meta.is_file() {
        return Err("solo se pueden enviar archivos (no carpetas)".into());
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(sanitize_name)
        .ok_or("nombre de archivo no válido")?;
    let size = meta.len();
    let header = FileHeader { name, size }
        .encode()
        .map_err(|e| e.to_string())?;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("no se puede abrir: {e}"))?;
    let mut send = conn
        .open_uni()
        .await
        .map_err(|e| format!("conexión: {e}"))?;
    let net = |e: quinn::WriteError| format!("conexión: {e}");
    send.write_all(&header).await.map_err(net)?;
    let mut buf = vec![0u8; CHUNK];
    let start = Instant::now();
    let mut sent = 0u64;
    while sent < size {
        let want = CHUNK.min((size - sent) as usize);
        let n = file
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("lectura: {e}"))?;
        if n == 0 {
            return Err("el archivo se acortó mientras se enviaba".into());
        }
        send.write_all(&buf[..n]).await.map_err(net)?;
        sent += n as u64;
        // Límite de velocidad: no adelantarse al ritmo permitido.
        let due = Duration::from_secs_f64(sent as f64 / RATE_BYTES_PER_SEC as f64);
        let elapsed = start.elapsed();
        if due > elapsed {
            tokio::time::sleep(due - elapsed).await;
        }
    }
    send.finish().map_err(|e| format!("conexión: {e}"))?;
    // Esperar a que el par lo haya recibido todo (o lo rechace).
    match send.stopped().await {
        Ok(None) => Ok(sent),
        Ok(Some(code)) => Err(format!("el otro equipo lo rechazó (código {code})")),
        Err(e) => Err(format!("conexión: {e}")),
    }
}

/// Recibe un archivo de `recv` en `dir`. Devuelve (nombre, ruta, tamaño), o
/// (nombre, error) si falla; en ese caso no deja restos.
pub async fn receive_file(
    mut recv: quinn::RecvStream,
    dir: &Path,
) -> Result<(String, PathBuf, u64), (String, String)> {
    let header = tokio::time::timeout(HEADER_TIMEOUT, read_header(&mut recv))
        .await
        .map_err(|_| ("?".to_owned(), "cabecera no recibida a tiempo".to_owned()))?
        .map_err(|e| ("?".to_owned(), e))?;
    let raw_name = header.name.clone();
    let fail = |e: String| (raw_name.clone(), e);
    let name = sanitize_name(&header.name).ok_or_else(|| fail("nombre no válido".into()))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| fail(format!("no se puede crear {}: {e}", dir.display())))?;
    let part = unique_path(dir, &format!(".{name}.part"));
    let res = write_body(&mut recv, &part, header.size).await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&part).await;
        let _ = recv.stop(1u32.into());
        return Err(fail(e));
    }
    let target = unique_path(dir, &name);
    tokio::fs::rename(&part, &target).await.map_err(|e| {
        let _ = std::fs::remove_file(&part);
        fail(format!("no se pudo guardar: {e}"))
    })?;
    mark_downloaded(&target);
    Ok((name, target, header.size))
}

async fn read_header(recv: &mut quinn::RecvStream) -> Result<FileHeader, String> {
    let mut prefix = [0u8; 6];
    recv.read_exact(&mut prefix)
        .await
        .map_err(|e| format!("cabecera: {e}"))?;
    let n = FileHeader::decode_prefix(&prefix).map_err(|e| e.to_string())?;
    let mut name = vec![0u8; n];
    recv.read_exact(&mut name)
        .await
        .map_err(|e| format!("cabecera: {e}"))?;
    let mut size = [0u8; 8];
    recv.read_exact(&mut size)
        .await
        .map_err(|e| format!("cabecera: {e}"))?;
    FileHeader::decode_rest(&name, size).map_err(|e| e.to_string())
}

async fn write_body(recv: &mut quinn::RecvStream, part: &Path, size: u64) -> Result<(), String> {
    let mut file = tokio::fs::File::create(part)
        .await
        .map_err(|e| format!("no se puede escribir {}: {e}", part.display()))?;
    let mut got = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match recv.read(&mut buf).await {
            Ok(Some(n)) => n,
            Ok(None) => break,
            Err(e) => return Err(format!("conexión: {e}")),
        };
        got += n as u64;
        if got > size {
            return Err("el par envió más datos de los anunciados".into());
        }
        file.write_all(&buf[..n])
            .await
            .map_err(|e| format!("escritura: {e}"))?;
    }
    if got != size {
        return Err(format!("incompleto: {got} de {size} bytes"));
    }
    file.flush().await.map_err(|e| format!("escritura: {e}"))?;
    file.sync_all()
        .await
        .map_err(|e| format!("escritura: {e}"))?;
    Ok(())
}

/// Windows: *Mark of the Web* (zona Internet) para que el sistema avise antes
/// de abrir un ejecutable recibido. En otros sistemas no hace nada.
fn mark_downloaded(path: &Path) {
    #[cfg(windows)]
    {
        let mut ads = path.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&ads, "[ZoneTransfer]\r\nZoneId=3\r\n") {
            tracing::debug!("sin Mark of the Web en {}: {e}", path.display());
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_paths_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("cs-files-uniq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        std::fs::write(dir.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("README"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "README"), dir.join("README (1)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn xdg_download_dir_is_parsed() {
        let home = Path::new("/home/ana");
        let s = "# comentario\nXDG_DESKTOP_DIR=\"$HOME/Escritorio\"\nXDG_DOWNLOAD_DIR=\"$HOME/Descargas\"\n";
        assert_eq!(
            xdg_download_dir(s, home),
            Some(PathBuf::from("/home/ana/Descargas"))
        );
        assert_eq!(
            xdg_download_dir("XDG_DOWNLOAD_DIR=\"/datos/bajadas\"", home),
            Some(PathBuf::from("/datos/bajadas"))
        );
        // `$HOME` a secas significa "desactivado": no usar el home.
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", home), None);
        assert_eq!(xdg_download_dir("", home), None);
    }
}
