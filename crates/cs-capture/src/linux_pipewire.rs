//! Consumo de un stream de vídeo PipeWire obtenido desde XDG Portal.
//!
//! El bucle de PipeWire vive en un hilo dedicado. Los buffers se copian a un
//! canal acotado para que la captura nunca acumule latencia si el codificador
//! se retrasa.

use pipewire as pw;
use pw::spa;
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

impl PipeWireCapture {
    pub fn into_parts(self) -> (mpsc::Receiver<I420Buffer>, PipeWireCaptureControl) {
        (self.frames, self.control)
    }
}

impl PipeWireCaptureControl {
    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PipeWireCaptureControl {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Abre un nodo de captura ya autorizado por Portal.
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
            let result = run_i420(node_id, fd, tx, stop_rx);
            let _ = ready_tx.send(result);
        })
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?;
    match ready_rx
        .recv()
        .map_err(|e| PipeWireCaptureError::Format(e.to_string()))?
    {
        Ok(()) => Ok(PipeWireCapture {
            frames: rx,
            control: PipeWireCaptureControl {
                stop: stop_tx,
                join: Some(join),
            },
        }),
        Err(error) => {
            let _ = join.join();
            Err(error)
        }
    }
}

fn run_i420(
    node_id: u32,
    fd: std::os::fd::OwnedFd,
    tx: mpsc::SyncSender<I420Buffer>,
    stop_rx: pw::channel::Receiver<()>,
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
    let dimensions = Arc::new(Mutex::new(None::<(u32, u32)>));
    let dimensions_for_params = Arc::clone(&dimensions);
    let dimensions_for_process = Arc::clone(&dimensions);
    let _listener = stream
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
            let mut format = pw::spa::param::video::VideoInfoRaw::default();
            if format.parse(param).is_ok() {
                if let Ok(mut current) = dimensions_for_params.lock() {
                    *current = Some((format.size().width, format.size().height));
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let size = data.chunk().size() as usize;
            let first = data
                .data()
                .map(|slice| slice[..size.min(slice.len())].to_vec())
                .unwrap_or_default();
            if first.is_empty() {
                return;
            }
            let (width, height) = dimensions_for_process
                .lock()
                .ok()
                .and_then(|current| *current)
                .unwrap_or((0, 0));
            let half_height = height as usize / 2;
            let y_len = width as usize * height as usize;
            let c_len = width as usize / 2 * half_height;
            let (y, u, v, strides) = if buffer.datas_mut().len() >= 3 {
                let planes = buffer.datas_mut();
                let copy_plane = |plane: &mut spa::data::Data| {
                    let size = plane.chunk().size() as usize;
                    plane
                        .data()
                        .map(|slice| slice[..size.min(slice.len())].to_vec())
                        .unwrap_or_default()
                };
                let y = copy_plane(&mut planes[0]);
                let u = copy_plane(&mut planes[1]);
                let v = copy_plane(&mut planes[2]);
                (
                    y,
                    u,
                    v,
                    (width as usize, width as usize / 2, width as usize / 2),
                )
            } else if first.len() >= y_len.saturating_add(c_len.saturating_mul(2)) {
                (
                    first[..y_len].to_vec(),
                    first[y_len..y_len + c_len].to_vec(),
                    first[y_len + c_len..y_len + c_len * 2].to_vec(),
                    (width as usize, width as usize / 2, width as usize / 2),
                )
            } else {
                return;
            };
            let _ = tx.try_send(I420Buffer {
                width,
                height,
                y,
                u,
                v,
                strides,
            });
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
            pw::spa::param::video::VideoFormat::I420,
            pw::spa::param::video::VideoFormat::I420
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
                width: 4096,
                height: 4096
            }
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 1, denom: 1 },
            pw::spa::utils::Fraction { num: 120, denom: 1 }
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
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    let stop_loop = main_loop.clone();
    let _stop_listener = stop_rx.attach(main_loop.as_ref(), move |_| stop_loop.quit());
    let _stream = stream;
    let _listener = _listener;
    main_loop.run();
    Ok(())
}
