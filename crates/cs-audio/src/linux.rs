//! Backend de Linux con PipeWire (captura del audio del sistema y
//! reproducción). Se pide el formato interno (48 kHz, estéreo, `f32`) y el
//! adaptador de PipeWire convierte lo que haga falta.
//!
//! - Captura: flujo de entrada sobre el *monitor* del sumidero por defecto
//!   (`stream.capture.sink = true`): lo que suena en el equipo.
//! - Reproducción: flujo de salida que cada ciclo entrega lo que pide
//!   PipeWire (`pw_buffer.requested`) desde el [`JitterBuffer`].

#![allow(unsafe_code)] // FFI mínima de PipeWire (buffer en crudo); cada bloque está acotado y comentado.

use crate::jitter::JitterBuffer;
use crate::resample::FrameAssembler;
use crate::{AudioError, CHANNELS, SAMPLE_RATE};
use pipewire as pw;
use pw::spa;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

const STRIDE: usize = std::mem::size_of::<f32>() * CHANNELS;

fn dev(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// Hilo con su propio bucle de PipeWire y canal de parada.
struct PwThread {
    stop: pw::channel::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PwThread {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Pod de formato: F32LE intercalado, 48 kHz, estéreo (FL FR).
fn format_pod() -> Result<Vec<u8>, AudioError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0u32; 64];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| dev(format!("formato: {e:?}")))?
    .0
    .into_inner())
}

/// Arranca un hilo que crea el flujo con `build` y corre el bucle; espera a
/// que el flujo esté conectado para devolver errores de arranque.
fn spawn_stream<F>(name: &str, build: F) -> Result<PwThread, AudioError>
where
    F: FnOnce(&pw::MainLoop, &pw::Core) -> Result<Box<dyn std::any::Any>, AudioError>
        + Send
        + 'static,
{
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);
    let join = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let run = || -> Result<(), AudioError> {
                pw::init();
                let main_loop = pw::MainLoop::new().map_err(dev)?;
                let context = pw::Context::new(&main_loop).map_err(dev)?;
                let core = context.connect(None).map_err(dev)?;
                let _keep = build(&main_loop, &core)?;
                let quit = main_loop.clone();
                let _stop = stop_rx.attach(main_loop.as_ref(), move |_| quit.quit());
                let _ = ready_tx.send(Ok(()));
                main_loop.run();
                Ok(())
            };
            if let Err(e) = run() {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(dev)?;
    let th = PwThread {
        stop: stop_tx,
        join: Some(join),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(th),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(dev("el hilo de PipeWire terminó al arrancar")),
    }
}

/// Captura del audio del sistema en tramas de 10 ms (48 kHz estéreo).
pub struct AudioCapture {
    frames: mpsc::Receiver<Vec<f32>>,
    _thread: PwThread,
}

impl std::fmt::Debug for AudioCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioCapture")
    }
}

impl AudioCapture {
    /// Siguiente trama; `None` si no llegó ninguna en `timeout` (silencio:
    /// PipeWire no entrega nada cuando no suena nada) o si se cerró.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<Vec<f32>> {
        self.frames.recv_timeout(timeout).ok()
    }
}

pub fn start_capture() -> Result<AudioCapture, AudioError> {
    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(32);
    let thread = spawn_stream("cs-audio-captura", move |_ml, core| {
        let stream = pw::stream::Stream::new(
            core,
            "crossscreen-audio-captura",
            pw::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                *pw::keys::NODE_LATENCY => "480/48000",
            },
        )
        .map_err(dev)?;
        let mut assembler = FrameAssembler::default();
        let mut tmp: Vec<f32> = Vec::new();
        let listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let size = data.chunk().size() as usize;
                let offset = data.chunk().offset() as usize;
                let Some(bytes) = data.data() else {
                    return;
                };
                let end = (offset + size).min(bytes.len());
                if offset >= end {
                    return;
                }
                tmp.clear();
                tmp.extend(
                    bytes[offset..end]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
                assembler.push(&tmp);
                while let Some(frame) = assembler.next_frame() {
                    // Si el codificador va atrasado, se descarta (no se acumula).
                    let _ = tx.try_send(frame);
                }
            })
            .register()
            .map_err(dev)?;
        let pod = format_pod()?;
        let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| dev("pod de formato"))?];
        stream
            .connect(
                spa::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(dev)?;
        Ok(Box::new((stream, listener)))
    })?;
    Ok(AudioCapture {
        frames: rx,
        _thread: thread,
    })
}

/// Reproducción desde un búfer de jitter compartido.
pub struct AudioPlayback {
    _thread: PwThread,
}

impl std::fmt::Debug for AudioPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioPlayback")
    }
}

pub fn start_playback(jitter: Arc<Mutex<JitterBuffer>>) -> Result<AudioPlayback, AudioError> {
    let thread = spawn_stream("cs-audio-reproduccion", move |_ml, core| {
        let stream = pw::stream::Stream::new(
            core,
            "crossscreen-audio",
            pw::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::NODE_LATENCY => "480/48000",
            },
        )
        .map_err(dev)?;
        let mut pcm: Vec<f32> = Vec::new();
        let listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                // SAFETY: búfer obtenido del propio flujo y devuelto con
                // `queue_raw_buffer` en el mismo callback; solo se escriben
                // los bytes de `datas[0]` dentro de `maxsize`.
                unsafe {
                    let raw = stream.dequeue_raw_buffer();
                    if raw.is_null() {
                        return;
                    }
                    let spa_buf = (*raw).buffer;
                    if spa_buf.is_null() || (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
                        stream.queue_raw_buffer(raw);
                        return;
                    }
                    let d = &mut *(*spa_buf).datas;
                    if d.data.is_null() || d.chunk.is_null() {
                        stream.queue_raw_buffer(raw);
                        return;
                    }
                    let max_frames = d.maxsize as usize / STRIDE;
                    let requested = (*raw).requested as usize;
                    let frames = if requested > 0 {
                        requested.min(max_frames)
                    } else {
                        max_frames.min(480)
                    };
                    pcm.clear();
                    pcm.resize(frames * CHANNELS, 0.0);
                    if let Ok(mut j) = jitter.lock() {
                        j.pull(&mut pcm);
                    }
                    let out = std::slice::from_raw_parts_mut(d.data as *mut u8, frames * STRIDE);
                    for (dst, v) in out.chunks_exact_mut(4).zip(pcm.iter()) {
                        dst.copy_from_slice(&v.to_le_bytes());
                    }
                    (*d.chunk).offset = 0;
                    (*d.chunk).stride = STRIDE as i32;
                    (*d.chunk).size = (frames * STRIDE) as u32;
                    (*raw).size = frames as u64;
                    stream.queue_raw_buffer(raw);
                }
            })
            .register()
            .map_err(dev)?;
        let pod = format_pod()?;
        let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| dev("pod de formato"))?];
        stream
            .connect(
                spa::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(dev)?;
        Ok(Box::new((stream, listener)))
    })?;
    Ok(AudioPlayback { _thread: thread })
}
