//! Consumo de un stream de vídeo PipeWire obtenido desde XDG Portal.
//!
//! El bucle de PipeWire vive en un hilo dedicado que solo **copia** cada
//! buffer a una ranura de un frame ([`LatestFrame`]): el más nuevo sustituye
//! al que el codificador aún no recogió. Así la captura nunca acumula
//! latencia si el codificador se retrasa, el último cambio de la pantalla
//! nunca se pierde (antes se descartaba el nuevo y se codificaba el viejo) y
//! no se gasta CPU en frames que nadie va a enviar.
//!
//! Se negocian los formatos RGB empaquetados que ofrecen los compositores
//! (GNOME/Mutter: `BGRx`/`BGRA`; KDE y wlroots: también `RGBx`/`xRGB`…) e
//! I420; los RGB se convierten a I420 BT.709 al recoger el frame, en el hilo
//! del codificador.

use crate::yuv::{conversion_threads, PackedRgb};
use crate::yuv_pool::ConvertPool;
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct I420Buffer {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub strides: (usize, usize, usize),
    /// Cuándo llegó el frame de la captura (para medir captura → envío).
    pub captured: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum PipeWireCaptureError {
    #[error("PipeWire: {0}")]
    PipeWire(#[from] pw::Error),
    #[error("no se pudo serializar el formato PipeWire: {0}")]
    Format(String),
}

pub struct PipeWireCapture {
    pub frames: LatestFrame,
    control: PipeWireCaptureControl,
}

/// Frame capturado que el codificador aún no ha recogido.
enum Pending {
    /// RGB empaquetado copiado tal cual (filas de `stride` bytes); se
    /// convierte a I420 al recogerlo.
    Packed {
        data: Vec<u8>,
        stride: usize,
        width: u32,
        height: u32,
        order: PackedRgb,
        captured: Instant,
    },
    /// El compositor ya lo entregó en I420.
    Ready(I420Buffer),
}

#[derive(Default)]
struct SlotState {
    pending: Option<Pending>,
    /// Búfer RGB ya usado, para no reservar memoria en cada frame (con poca
    /// memoria libre, reservar y soltar varios MB por frame cuesta).
    spare: Option<Vec<u8>>,
    /// El hilo de PipeWire terminó: no llegarán más frames.
    closed: bool,
    /// Frames sustituidos por uno más nuevo antes de que el codificador los
    /// recogiera.
    replaced: u64,
}

#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Deja `frame` como el pendiente; el anterior, si lo había, se descarta.
    fn put(&self, frame: Pending) {
        let mut st = self.lock();
        if let Some(old) = st.pending.replace(frame) {
            st.replaced += 1;
            if let Pending::Packed { data, .. } = old {
                st.spare = Some(data);
            }
        }
        drop(st);
        self.ready.notify_one();
    }

    /// Copia un frame RGB reutilizando un búfer anterior.
    fn put_packed(&self, src: &[u8], stride: usize, n: Negotiated, order: PackedRgb) {
        let captured = Instant::now();
        // Recuperar un búfer sin copiar bajo el candado: el pendiente que se
        // va a sustituir o el de repuesto.
        let mut data = {
            let mut st = self.lock();
            match st.pending.take() {
                Some(Pending::Packed { data, .. }) => {
                    st.replaced += 1;
                    data
                }
                Some(Pending::Ready(_)) => {
                    st.replaced += 1;
                    st.spare.take().unwrap_or_default()
                }
                None => st.spare.take().unwrap_or_default(),
            }
        };
        data.clear();
        data.extend_from_slice(src);
        self.put(Pending::Packed {
            data,
            stride,
            width: n.width,
            height: n.height,
            order,
            captured,
        });
    }

    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }
}

/// Extremo del codificador: el frame más reciente de la captura.
///
/// Se usa como un `mpsc::Receiver`: [`recv_timeout`](Self::recv_timeout)
/// devuelve `Disconnected` cuando el stream de PipeWire termina.
pub struct LatestFrame {
    slot: Arc<Slot>,
    /// Hilos fijos de conversión, creados al primer frame RGB en el hilo
    /// que recoge (el del codificador).
    pool: Option<ConvertPool>,
}

impl std::fmt::Debug for LatestFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatestFrame")
            .field("replaced", &self.replaced())
            .finish_non_exhaustive()
    }
}

impl LatestFrame {
    /// Espera hasta `timeout` al siguiente frame y lo devuelve en I420 (la
    /// conversión, si hace falta, ocurre aquí, en el hilo que llama).
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<I420Buffer, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            let pending = {
                let mut st = self.slot.lock();
                loop {
                    if let Some(p) = st.pending.take() {
                        break p;
                    }
                    if st.closed {
                        return Err(RecvTimeoutError::Disconnected);
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    st = self
                        .slot
                        .ready
                        .wait_timeout(st, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            match pending {
                Pending::Ready(frame) => return Ok(frame),
                Pending::Packed {
                    data,
                    stride,
                    width,
                    height,
                    order,
                    captured,
                } => {
                    let pixels = width as usize * height as usize;
                    let pool = match self.pool.take() {
                        Some(p) if p.threads() == conversion_threads(pixels) => p,
                        _ => ConvertPool::for_pixels(pixels),
                    };
                    let pool = self.pool.insert(pool);
                    let (data, planes) = pool.convert(data, stride, width, height, order);
                    {
                        let mut st = self.slot.lock();
                        if st.spare.is_none() {
                            st.spare = Some(data);
                        }
                    }
                    // Tamaño ya validado al copiar; si aun así no cuadra, se
                    // ignora este frame y se espera al siguiente.
                    if let Some(p) = planes {
                        let (w, cw) = (width as usize, (width as usize).div_ceil(2));
                        return Ok(I420Buffer {
                            width,
                            height,
                            y: p.y,
                            u: p.u,
                            v: p.v,
                            strides: (w, cw, cw),
                            captured,
                        });
                    }
                }
            }
        }
    }

    /// Frames que la captura sustituyó por uno más nuevo antes de que se
    /// recogieran (el codificador no daba abasto).
    pub fn replaced(&self) -> u64 {
        self.slot.lock().replaced
    }

    #[cfg(test)]
    fn pair() -> (Arc<Slot>, LatestFrame) {
        let slot = Arc::new(Slot::default());
        (Arc::clone(&slot), LatestFrame { slot, pool: None })
    }
}

pub struct PipeWireCaptureControl {
    stop: pw::channel::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for PipeWireCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireCapture").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PipeWireCaptureControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireCaptureControl")
            .finish_non_exhaustive()
    }
}

impl PipeWireCapture {
    pub fn into_parts(self) -> (LatestFrame, PipeWireCaptureControl) {
        (self.frames, self.control)
    }
}

impl PipeWireCaptureControl {
    /// Detiene el bucle de PipeWire y espera al hilo (lo hace `Drop`).
    pub fn stop(self) {}
}

impl Drop for PipeWireCaptureControl {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Abre un nodo de captura ya autorizado por Portal y entrega frames I420.
pub fn start_i420(
    node_id: u32,
    fd: std::os::fd::OwnedFd,
) -> Result<PipeWireCapture, PipeWireCaptureError> {
    start_i420_sized(node_id, fd, None)
}

/// Como [`start_i420`], pero pidiendo un tamaño fijo. Con un monitor virtual
/// de Mutter, ese tamaño es el que tendrá el monitor.
pub fn start_i420_sized(
    node_id: u32,
    fd: std::os::fd::OwnedFd,
    fixed_size: Option<(u32, u32)>,
) -> Result<PipeWireCapture, PipeWireCaptureError> {
    let slot = Arc::new(Slot::default());
    let frames = LatestFrame {
        slot: Arc::clone(&slot),
        pool: None,
    };
    let (stop_tx, stop_rx) = pw::channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let join = std::thread::Builder::new()
        .name("crossscreen-pipewire".into())
        .spawn(move || {
            /// Al salir el hilo (también por pánico) el consumidor ve
            /// `Disconnected` y la sesión termina con causa.
            struct CloseOnExit(Arc<Slot>);
            impl Drop for CloseOnExit {
                fn drop(&mut self) {
                    self.0.close();
                }
            }
            let _close = CloseOnExit(Arc::clone(&slot));
            // Que el compositor nunca espere por este hilo aunque la CPU
            // esté saturada.
            crate::linux_priority::raise_current_thread_logged("captura PipeWire");
            // `run_i420` avisa con `Ok` justo antes de entrar en el bucle; si
            // falla antes, el error se envía aquí.
            if let Err(e) = run_i420(node_id, fd, fixed_size, slot, stop_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?;
    let control = PipeWireCaptureControl {
        stop: stop_tx,
        join: Some(join),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PipeWireCapture { frames, control }),
        Ok(Err(error)) => Err(error),
        Err(e) => Err(PipeWireCaptureError::Format(e.to_string())),
    }
}

/// Formato negociado: tamaño y formato de píxel.
#[derive(Clone, Copy)]
struct Negotiated {
    width: u32,
    height: u32,
    format: VideoFormat,
}

fn packed_order(format: VideoFormat) -> Option<PackedRgb> {
    if format == VideoFormat::BGRx || format == VideoFormat::BGRA {
        Some(PackedRgb::Bgrx)
    } else if format == VideoFormat::RGBx || format == VideoFormat::RGBA {
        Some(PackedRgb::Rgbx)
    } else if format == VideoFormat::xRGB || format == VideoFormat::ARGB {
        Some(PackedRgb::Xrgb)
    } else if format == VideoFormat::xBGR || format == VideoFormat::ABGR {
        Some(PackedRgb::Xbgr)
    } else {
        None
    }
}

/// Contenido válido (`offset..offset+size`) de un plano, sin copiar, y su stride.
fn plane_slice(data: &mut spa::data::Data, default_stride: usize) -> Option<(&[u8], usize)> {
    let chunk = data.chunk();
    let (offset, size, stride) = (
        chunk.offset() as usize,
        chunk.size() as usize,
        chunk.stride(),
    );
    let stride = if stride > 0 {
        stride as usize
    } else {
        default_stride
    };
    let slice = data.data()?;
    let end = offset.checked_add(size)?.min(slice.len());
    (offset < end).then(|| (&slice[offset..end], stride))
}

/// Como [`plane_slice`], pero copiando el plano.
fn plane_bytes(data: &mut spa::data::Data, default_stride: usize) -> Option<(Vec<u8>, usize)> {
    plane_slice(data, default_stride).map(|(s, stride)| (s.to_vec(), stride))
}

/// ¿El cambio de estado significa que el stream terminó? `Unconnected` es
/// también el estado inicial, así que solo cuenta si se viene de otro.
fn stream_ended(old: &pw::stream::StreamState, new: &pw::stream::StreamState) -> bool {
    use pw::stream::StreamState;
    match new {
        StreamState::Error(_) => true,
        StreamState::Unconnected => !matches!(old, StreamState::Unconnected),
        _ => false,
    }
}

/// ¿Caben `rows` filas de `width` bytes con este `stride` en `len` bytes?
fn plane_fits(len: usize, stride: usize, width: usize, rows: usize) -> bool {
    rows > 0
        && stride >= width
        && stride
            .checked_mul(rows - 1)
            .and_then(|n| n.checked_add(width))
            .is_some_and(|need| need <= len)
}

/// Deja un buffer de PipeWire en la ranura: los RGB empaquetados se copian
/// sin convertir; los I420, ya copiados en sus planos. `false` si el buffer
/// no tiene un formato o un tamaño válidos.
fn capture_into(datas: &mut [spa::data::Data], n: Negotiated, slot: &Slot) -> bool {
    if let Some(order) = packed_order(n.format) {
        let w = n.width as usize;
        let Some((bytes, stride)) = datas.first_mut().and_then(|d| plane_slice(d, w * 4)) else {
            return false;
        };
        if !plane_fits(bytes.len(), stride, w * 4, n.height as usize) {
            return false;
        }
        slot.put_packed(bytes, stride, n, order);
        return true;
    }
    match to_i420(datas, n) {
        Some(frame) => {
            slot.put(Pending::Ready(frame));
            true
        }
        None => false,
    }
}

/// Copia un buffer I420 de PipeWire en un [`I420Buffer`].
fn to_i420(datas: &mut [spa::data::Data], n: Negotiated) -> Option<I420Buffer> {
    let (w, h) = (n.width as usize, n.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if n.format != VideoFormat::I420 {
        return None;
    }
    if datas.len() >= 3 {
        let (y, ys) = plane_bytes(&mut datas[0], w)?;
        let (u, us) = plane_bytes(&mut datas[1], cw)?;
        let (v, vs) = plane_bytes(&mut datas[2], cw)?;
        if !(plane_fits(y.len(), ys, w, h)
            && plane_fits(u.len(), us, cw, ch)
            && plane_fits(v.len(), vs, cw, ch))
        {
            return None;
        }
        return Some(I420Buffer {
            width: n.width,
            height: n.height,
            y,
            u,
            v,
            strides: (ys, us, vs),
            captured: Instant::now(),
        });
    }
    // Un solo bloque con los tres planos contiguos y sin relleno.
    let (all, _) = plane_bytes(datas.first_mut()?, w)?;
    let (y_len, c_len) = (w * h, cw * ch);
    if all.len() < y_len + 2 * c_len {
        return None;
    }
    Some(I420Buffer {
        width: n.width,
        height: n.height,
        y: all[..y_len].to_vec(),
        u: all[y_len..y_len + c_len].to_vec(),
        v: all[y_len + c_len..y_len + 2 * c_len].to_vec(),
        strides: (w, cw, cw),
        captured: Instant::now(),
    })
}

fn run_i420(
    node_id: u32,
    fd: std::os::fd::OwnedFd,
    fixed_size: Option<(u32, u32)>,
    slot: Arc<Slot>,
    stop_rx: pw::channel::Receiver<()>,
    ready: &mpsc::SyncSender<Result<(), PipeWireCaptureError>>,
) -> Result<(), PipeWireCaptureError> {
    pw::init();
    let main_loop = pw::MainLoop::new()?;
    let context = pw::Context::new(&main_loop)?;
    let core = context.connect_fd(fd, None)?;
    let stream = pw::stream::Stream::new(
        &core,
        "crossscreen-screen-capture",
        pw::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let negotiated = Arc::new(Mutex::new(None::<Negotiated>));
    let negotiated_for_params = Arc::clone(&negotiated);
    // Si el compositor cierra el stream (p. ej. «Dejar de compartir» en
    // GNOME) o PipeWire informa de un error, se sale del bucle: al terminar
    // el hilo se cierra la ranura y la sesión termina con causa.
    let quit_on_end = main_loop.clone();
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |old, new| {
            tracing::info!("PipeWire: estado {old:?} → {new:?}");
            if stream_ended(&old, &new) {
                tracing::warn!("PipeWire: el stream terminó ({new:?})");
                quit_on_end.quit();
            }
        })
        .param_changed(move |_, id, _, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::format::MediaType::Video
                || media_subtype != pw::spa::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = pw::spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_ok() {
                let n = Negotiated {
                    width: info.size().width,
                    height: info.size().height,
                    format: info.format(),
                };
                tracing::info!("PipeWire: {}x{}, formato {:?}", n.width, n.height, n.format);
                if let Ok(mut current) = negotiated_for_params.lock() {
                    *current = Some(n);
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(n) = negotiated.lock().ok().and_then(|c| *c) else {
                return;
            };
            if n.width == 0 || n.height == 0 {
                return;
            }
            // Ranura de un frame: si el codificador va atrasado, este
            // sustituye al que no llegó a recoger.
            let _ = capture_into(buffer.datas_mut(), n, &slot);
        })
        .register()
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?;
    // Tamaño fijo (monitor virtual: será su resolución) o cualquiera.
    let size_prop = match fixed_size {
        Some((width, height)) => pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoSize,
            Rectangle,
            pw::spa::utils::Rectangle { width, height }
        ),
        None => pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 320,
                height: 240
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
    };
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::MediaType,
            Id,
            pw::spa::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::xRGB,
            VideoFormat::ARGB,
            VideoFormat::xBGR,
            VideoFormat::ABGR,
            VideoFormat::I420
        ),
        size_prop,
        // Mutter anuncia 0/1 (frecuencia variable): el mínimo debe incluirlo.
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 360, denom: 1 }
        ),
    );
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| PipeWireCaptureError::Format("formato PipeWire inválido".into()))?;
    let mut params = [pod];
    stream.connect(
        spa::Direction::Input,
        Some(node_id),
        // Sin RT_PROCESS: la copia del frame corre en este hilo, no en el
        // hilo de tiempo real de PipeWire.
        // DONT_RECONNECT: si la fuente desaparece (p. ej. «Dejar de compartir»)
        // el stream pasa a desconectado en vez de esperar otra fuente.
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::DONT_RECONNECT,
        &mut params,
    )?;
    let stop_loop = main_loop.clone();
    let _stop_listener = stop_rx.attach(main_loop.as_ref(), move |_| stop_loop.quit());
    // Avisar antes de entrar en el bucle: `start_i420` espera esta señal.
    let _ = ready.send(Ok(()));
    main_loop.run();
    drop(listener);
    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{plane_fits, stream_ended, LatestFrame, Negotiated};
    use crate::yuv::PackedRgb;
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::stream::StreamState;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    fn bgrx(w: u32, h: u32, value: u8) -> (Vec<u8>, Negotiated) {
        (
            vec![value; (w * h * 4) as usize],
            Negotiated {
                width: w,
                height: h,
                format: VideoFormat::BGRx,
            },
        )
    }

    /// El codificador recoge siempre el frame más nuevo: los que no llegó a
    /// recoger se sustituyen y se cuentan.
    #[test]
    fn newest_frame_replaces_the_pending_one() {
        let (slot, mut rx) = LatestFrame::pair();
        for value in [10u8, 20, 200] {
            let (data, n) = bgrx(4, 2, value);
            slot.put_packed(&data, 16, n, PackedRgb::Bgrx);
        }
        let got = rx.recv_timeout(Duration::from_millis(10)).unwrap();
        assert_eq!((got.width, got.height), (4, 2));
        // Gris 200 en BT.709 limitado: Y = 16 + 200·219/255 ≈ 188.
        assert!(
            got.y.iter().all(|&y| (187..=189).contains(&y)),
            "{:?}",
            got.y
        );
        assert_eq!(rx.replaced(), 2);
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(5)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn closing_wakes_the_consumer_after_the_last_frame() {
        let (slot, mut rx) = LatestFrame::pair();
        let (data, n) = bgrx(2, 2, 0);
        slot.put_packed(&data, 8, n, PackedRgb::Bgrx);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            slot.close();
        });
        // El frame pendiente se entrega aunque luego se cierre.
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        ));
        closer.join().unwrap();
    }

    #[test]
    fn rgb_buffers_are_reused() {
        let (slot, mut rx) = LatestFrame::pair();
        let (data, n) = bgrx(64, 64, 50);
        slot.put_packed(&data, 256, n, PackedRgb::Bgrx);
        rx.recv_timeout(Duration::from_millis(10)).unwrap();
        let spare = slot.lock().spare.as_ref().map(Vec::as_ptr);
        assert!(spare.is_some());
        slot.put_packed(&data, 256, n, PackedRgb::Bgrx);
        let reused = match slot.lock().pending.as_ref() {
            Some(super::Pending::Packed { data, .. }) => Some(data.as_ptr()),
            _ => None,
        };
        assert_eq!(spare, reused);
    }

    #[test]
    fn stream_end_detection() {
        assert!(!stream_ended(
            &StreamState::Unconnected,
            &StreamState::Unconnected
        ));
        assert!(!stream_ended(
            &StreamState::Unconnected,
            &StreamState::Connecting
        ));
        assert!(!stream_ended(&StreamState::Paused, &StreamState::Streaming));
        assert!(stream_ended(
            &StreamState::Streaming,
            &StreamState::Unconnected
        ));
        assert!(stream_ended(
            &StreamState::Paused,
            &StreamState::Error("x".into())
        ));
    }

    #[test]
    fn plane_bounds() {
        assert!(plane_fits(1920 * 1080, 1920, 1920, 1080));
        assert!(plane_fits(2048 * 1079 + 1920, 2048, 1920, 1080));
        assert!(!plane_fits(2048 * 1079 + 1919, 2048, 1920, 1080));
        assert!(!plane_fits(1 << 30, 1000, 1920, 1080));
        assert!(!plane_fits(100, 1920, 1920, 0));
    }
}
