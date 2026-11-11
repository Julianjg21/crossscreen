//! Captura de una salida de un compositor wlroots (Sway, Hyprland…) con el
//! protocolo `wlr-screencopy-unstable-v1`, sin portal (ADR-0011).
//!
//! Sirve para el monitor virtual de Sway: el portal de wlroots solo deja
//! elegir la salida por su archivo de configuración o con un selector
//! interactivo, y aquí la salida la acaba de crear CrossScreen.
//!
//! Un hilo dedicado:
//! - pide cada frame a la salida (`copy_with_damage`: solo cuando cambia);
//! - lo copia a memoria compartida (`wl_shm`);
//! - lo convierte a I420 BT.709;
//! - lo entrega por un canal de capacidad 1 (si el codificador va atrasado,
//!   se descarta).

use crate::linux_pipewire::I420Buffer;
use crate::yuv::{packed_to_i420, PackedRgb};
use rustix::event::{poll, PollFd, PollFlags};
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Debug, thiserror::Error)]
pub enum WlrCaptureError {
    #[error("no se pudo conectar con el compositor Wayland: {0}")]
    Connect(String),
    #[error("el compositor no ofrece {0}")]
    Missing(&'static str),
    #[error("no existe la salida {0}")]
    NoOutput(String),
    #[error("formato de píxel no admitido: {0:?}")]
    Format(WEnum<wl_shm::Format>),
    #[error("Wayland: {0}")]
    Wayland(String),
    #[error("el compositor rechazó la captura")]
    Failed,
}

fn wl<E: std::fmt::Display>(e: E) -> WlrCaptureError {
    WlrCaptureError::Wayland(e.to_string())
}

/// Al soltarlo se detiene el hilo de captura.
pub struct WlrCaptureControl {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for WlrCaptureControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrCaptureControl").finish_non_exhaustive()
    }
}

impl Drop for WlrCaptureControl {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Empieza a capturar la salida `output_name` (p. ej. `HEADLESS-1`) a un
/// máximo de `fps` frames por segundo.
pub fn start_i420(
    output_name: &str,
    fps: u32,
) -> Result<(mpsc::Receiver<I420Buffer>, WlrCaptureControl), WlrCaptureError> {
    let (tx, rx) = mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let name = output_name.to_owned();
    let thread_stop = stop.clone();
    let join = std::thread::Builder::new()
        .name("crossscreen-wlr-capture".into())
        .spawn(move || {
            let mut cap = match Capturer::open(&name) {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = cap.run(&tx, &thread_stop, fps.max(1)) {
                tracing::warn!("captura wlroots terminada: {e}");
            }
        })
        .map_err(wl)?;
    let control = WlrCaptureControl {
        stop,
        join: Some(join),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((rx, control)),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(wl(e)),
    }
}

/// ¿Hay un compositor con `wlr-screencopy` en esta sesión?
pub fn is_available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<State>(&conn) else {
        return false;
    };
    globals
        .contents()
        .clone_list()
        .iter()
        .any(|g| g.interface == "zwlr_screencopy_manager_v1")
}

#[derive(Default)]
struct FrameState {
    buffer: Option<(WEnum<wl_shm::Format>, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    frame: FrameState,
}

struct ShmBuffer {
    file: File,
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    key: (WEnum<wl_shm::Format>, u32, u32, u32),
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

struct Capturer {
    /// Mantiene viva la conexión con el compositor.
    _conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    shm: wl_shm::WlShm,
    manager: ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
    shm_buf: Option<ShmBuffer>,
    pixels: Vec<u8>,
}

impl Capturer {
    fn open(output_name: &str) -> Result<Self, WlrCaptureError> {
        let conn =
            Connection::connect_to_env().map_err(|e| WlrCaptureError::Connect(e.to_string()))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(wl)?;
        let qh = queue.handle();
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| WlrCaptureError::Missing("wl_shm"))?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|_| WlrCaptureError::Missing("wlr-screencopy"))?;
        let mut state = State::default();
        for g in globals.contents().clone_list() {
            // El nombre de la salida (`name`) llega desde wl_output v4.
            if g.interface == "wl_output" && g.version >= 4 {
                let o: wl_output::WlOutput = globals.registry().bind(g.name, 4, &qh, ());
                state.outputs.push((o, None));
            }
        }
        queue.roundtrip(&mut state).map_err(wl)?;
        let output = state
            .outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(output_name))
            .map(|(o, _)| o.clone())
            .ok_or_else(|| WlrCaptureError::NoOutput(output_name.to_owned()))?;
        Ok(Self {
            _conn: conn,
            queue,
            qh,
            state,
            shm,
            manager,
            output,
            shm_buf: None,
            pixels: Vec::new(),
        })
    }

    /// Despacha eventos esperando como mucho `timeout`.
    fn dispatch(&mut self, timeout: Duration) -> Result<(), WlrCaptureError> {
        self.queue.flush().map_err(wl)?;
        if self.queue.dispatch_pending(&mut self.state).map_err(wl)? > 0 {
            return Ok(());
        }
        if let Some(guard) = self.queue.prepare_read() {
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let ts = rustix::time::Timespec {
                    tv_sec: timeout.as_secs() as i64,
                    tv_nsec: i64::from(timeout.subsec_nanos()),
                };
                poll(&mut fds, Some(&ts)).map_err(wl)? > 0
            };
            if readable {
                guard.read().map_err(wl)?;
            }
        }
        self.queue.dispatch_pending(&mut self.state).map_err(wl)?;
        Ok(())
    }

    fn run(
        &mut self,
        tx: &mpsc::SyncSender<I420Buffer>,
        stop: &AtomicBool,
        fps: u32,
    ) -> Result<(), WlrCaptureError> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let version = self.manager.version();
        let mut first = true;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            self.state.frame = FrameState::default();
            let frame = self.manager.capture_output(1, &self.output, &self.qh, ());
            // 1. Formato del buffer.
            while !self.state.frame.failed
                && !(self.state.frame.buffer.is_some()
                    && (version < 3 || self.state.frame.buffer_done))
            {
                if stop.load(Ordering::Relaxed) {
                    frame.destroy();
                    return Ok(());
                }
                self.dispatch(Duration::from_millis(100))?;
            }
            if self.state.frame.failed {
                frame.destroy();
                return Err(WlrCaptureError::Failed);
            }
            let key = self.state.frame.buffer.expect("comprobado arriba");
            self.ensure_buffer(key)?;
            let buf = &self.shm_buf.as_ref().expect("recién creado").buffer;
            // 2. Copia: con daño (solo cuando cambia la imagen) salvo la primera.
            if version >= 2 && !first {
                frame.copy_with_damage(buf);
            } else {
                frame.copy(buf);
            }
            while !self.state.frame.ready && !self.state.frame.failed {
                if stop.load(Ordering::Relaxed) {
                    frame.destroy();
                    return Ok(());
                }
                self.dispatch(Duration::from_millis(100))?;
            }
            frame.destroy();
            if self.state.frame.failed {
                return Err(WlrCaptureError::Failed);
            }
            first = false;
            if let Some(i420) = self.convert(key)? {
                let _ = tx.try_send(i420);
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        Ok(())
    }

    fn ensure_buffer(
        &mut self,
        key: (WEnum<wl_shm::Format>, u32, u32, u32),
    ) -> Result<(), WlrCaptureError> {
        if self.shm_buf.as_ref().is_some_and(|b| b.key == key) {
            return Ok(());
        }
        self.shm_buf = None;
        let (format, width, height, stride) = key;
        let WEnum::Value(fmt) = format else {
            return Err(WlrCaptureError::Format(format));
        };
        let size = stride as usize * height as usize;
        let fd = rustix::fs::memfd_create("crossscreen-wlr", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(wl)?;
        let file = File::from(fd);
        file.set_len(size as u64).map_err(wl)?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            fmt,
            &self.qh,
            (),
        );
        self.shm_buf = Some(ShmBuffer {
            file,
            buffer,
            pool,
            key,
        });
        Ok(())
    }

    fn convert(
        &mut self,
        key: (WEnum<wl_shm::Format>, u32, u32, u32),
    ) -> Result<Option<I420Buffer>, WlrCaptureError> {
        let (format, width, height, stride) = key;
        let order = match format {
            // Little-endian: ARGB8888 en memoria es B G R A.
            WEnum::Value(wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) => PackedRgb::Bgrx,
            WEnum::Value(wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888) => PackedRgb::Rgbx,
            other => return Err(WlrCaptureError::Format(other)),
        };
        let size = stride as usize * height as usize;
        self.pixels.resize(size, 0);
        let file = &self.shm_buf.as_ref().expect("buffer creado").file;
        file.read_exact_at(&mut self.pixels, 0).map_err(wl)?;
        if self.state.frame.y_invert {
            let s = stride as usize;
            let h = height as usize;
            for row in 0..h / 2 {
                let (top, bottom) = self.pixels.split_at_mut((h - 1 - row) * s);
                top[row * s..row * s + s].swap_with_slice(&mut bottom[..s]);
            }
        }
        // H.264 4:2:0: dimensiones pares (se recorta la última fila/columna).
        let (w, h) = (width & !1, height & !1);
        let Some(p) = packed_to_i420(&self.pixels, stride as usize, w, h, order) else {
            return Ok(None);
        };
        let (wy, wc) = (w as usize, (w as usize).div_ceil(2));
        Ok(Some(I420Buffer {
            width: w,
            height: h,
            y: p.y,
            u: p.u,
            v: p.v,
            strides: (wy, wc, wc),
            captured: std::time::Instant::now(),
        }))
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = state.outputs.iter_mut().find(|(o, _)| o == output) {
                slot.1 = Some(name);
            }
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
ignore_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ZwlrScreencopyManagerV1
);

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let f = &mut state.frame;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => f.buffer = Some((format, width, height, stride)),
            Event::Flags { flags } => {
                f.y_invert = matches!(
                    flags,
                    WEnum::Value(v) if v.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
                );
            }
            Event::BufferDone => f.buffer_done = true,
            Event::Ready { .. } => f.ready = true,
            Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn y_invert_swaps_rows() {
        // Misma lógica que `convert`: 3 filas de 2 bytes.
        let mut px = vec![1, 1, 2, 2, 3, 3];
        let (s, h) = (2usize, 3usize);
        for row in 0..h / 2 {
            let (top, bottom) = px.split_at_mut((h - 1 - row) * s);
            top[row * s..row * s + s].swap_with_slice(&mut bottom[..s]);
        }
        assert_eq!(px, vec![3, 3, 2, 2, 1, 1]);
    }
}
