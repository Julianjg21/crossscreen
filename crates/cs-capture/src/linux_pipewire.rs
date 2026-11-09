//! Consumo de un stream de vídeo PipeWire obtenido desde XDG Portal.
//!
//! El bucle de PipeWire vive en un hilo dedicado. Los buffers se copian a un
//! canal acotado para que la captura nunca acumule latencia si el codificador
//! se retrasa.
//!
//! Se negocian los formatos RGB empaquetados que ofrecen los compositores
//! (GNOME/Mutter: `BGRx`/`BGRA`; KDE y wlroots: también `RGBx`/`xRGB`…) e
//! I420; los RGB se convierten a I420 BT.709 en el hilo de PipeWire.

use crate::yuv::{packed_to_i420, PackedRgb};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;
use std::sync::{mpsc, Arc, Mutex};

#[derive(Debug, Clone)]
pub struct I420Buffer {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub strides: (usize, usize, usize),
}

#[derive(Debug, thiserror::Error)]
pub enum PipeWireCaptureError {
    #[error("PipeWire: {0}")]
    PipeWire(#[from] pw::Error),
    #[error("no se pudo serializar el formato PipeWire: {0}")]
    Format(String),
}

pub struct PipeWireCapture {
    pub frames: mpsc::Receiver<I420Buffer>,
    control: PipeWireCaptureControl,
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
    pub fn into_parts(self) -> (mpsc::Receiver<I420Buffer>, PipeWireCaptureControl) {
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
    let (tx, rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = pw::channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let join = std::thread::Builder::new()
        .name("crossscreen-pipewire".into())
        .spawn(move || {
            // `run_i420` avisa con `Ok` justo antes de entrar en el bucle; si
            // falla antes, el error se envía aquí.
            if let Err(e) = run_i420(node_id, fd, tx, stop_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?;
    let control = PipeWireCaptureControl {
        stop: stop_tx,
        join: Some(join),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PipeWireCapture {
            frames: rx,
            control,
        }),
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

/// Copia el contenido válido (`offset..offset+size`) de un plano y su stride.
fn plane_bytes(data: &mut spa::data::Data, default_stride: usize) -> Option<(Vec<u8>, usize)> {
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
    (offset < end).then(|| (slice[offset..end].to_vec(), stride))
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

/// Convierte un buffer de PipeWire en [`I420Buffer`].
fn to_i420(datas: &mut [spa::data::Data], n: Negotiated) -> Option<I420Buffer> {
    let (w, h) = (n.width as usize, n.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if let Some(order) = packed_order(n.format) {
        let (bytes, stride) = plane_bytes(datas.first_mut()?, w * 4)?;
        let p = packed_to_i420(&bytes, stride, n.width, n.height, order)?;
        return Some(I420Buffer {
            width: n.width,
            height: n.height,
            y: p.y,
            u: p.u,
            v: p.v,
            strides: (w, cw, cw),
        });
    }
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
    })
}

fn run_i420(
    node_id: u32,
    fd: std::os::fd::OwnedFd,
    tx: mpsc::SyncSender<I420Buffer>,
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
    let listener = stream
        .add_local_listener_with_user_data(())
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
            if let Some(frame) = to_i420(buffer.datas_mut(), n) {
                // Canal de 1: si el codificador va atrasado se descarta el frame.
                let _ = tx.try_send(frame);
            }
        })
        .register()
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?;
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
        pw::spa::pod::property!(
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
        // Sin RT_PROCESS: la conversión a I420 corre en este hilo, no en el
        // hilo de tiempo real de PipeWire.
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
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
    use super::plane_fits;

    #[test]
    fn plane_bounds() {
        assert!(plane_fits(1920 * 1080, 1920, 1920, 1080));
        assert!(plane_fits(2048 * 1079 + 1920, 2048, 1920, 1080));
        assert!(!plane_fits(2048 * 1079 + 1919, 2048, 1920, 1080));
        assert!(!plane_fits(1 << 30, 1000, 1920, 1080));
        assert!(!plane_fits(100, 1920, 1920, 0));
    }
}
