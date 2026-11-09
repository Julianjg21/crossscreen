//! Backend de Windows con WASAPI en modo compartido.
//!
//! - Captura: *loopback* del dispositivo de salida por defecto (lo que suena).
//! - Reproducción: dirigida por eventos; cada vez que el dispositivo pide
//!   datos se sacan del [`JitterBuffer`].
//!
//! El formato del dispositivo (frecuencia, canales, `f32` o `i16`) se
//! convierte a y desde el interno (48 kHz estéreo `f32`).

#![allow(unsafe_code)] // FFI COM de WASAPI; cada bloque está acotado y comentado.

use crate::jitter::JitterBuffer;
use crate::resample::{from_stereo, to_stereo, FrameAssembler, Resampler};
use crate::{AudioError, CHANNELS, FRAME_LEN, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// `AUDCLNT_BUFFERFLAGS_SILENT`: el búfer capturado debe tratarse como silencio.
const BUFFERFLAGS_SILENT: u32 = 2;
// La conversión de canales asume estéreo interno.
const _: () = assert!(CHANNELS == 2);

fn dev(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// Formato de muestras del dispositivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
}

#[derive(Debug, Clone, Copy)]
struct DeviceFormat {
    rate: u32,
    channels: usize,
    kind: SampleKind,
}

/// Lee el `WAVEFORMATEX` (o `WAVEFORMATEXTENSIBLE`) que devuelve WASAPI.
///
/// # Safety
/// `pwfx` debe apuntar a un formato válido devuelto por `GetMixFormat`.
unsafe fn parse_format(pwfx: *const WAVEFORMATEX) -> Result<DeviceFormat, AudioError> {
    let f = *pwfx;
    let kind = match (f.wFormatTag, f.wBitsPerSample) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleKind::F32,
        (WAVE_FORMAT_PCM, 16) => SampleKind::I16,
        (WAVE_FORMAT_EXTENSIBLE, bits) => {
            let ext = *(pwfx as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            if sub == SUBTYPE_IEEE_FLOAT && bits == 32 {
                SampleKind::F32
            } else if sub == SUBTYPE_PCM && bits == 16 {
                SampleKind::I16
            } else {
                return Err(dev(format!(
                    "formato del dispositivo no soportado ({bits} bits)"
                )));
            }
        }
        (tag, bits) => {
            return Err(dev(format!(
                "formato del dispositivo no soportado (tag {tag}, {bits} bits)"
            )))
        }
    };
    Ok(DeviceFormat {
        rate: f.nSamplesPerSec,
        channels: usize::from(f.nChannels),
        kind,
    })
}

/// COM en este hilo durante la vida del valor.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, AudioError> {
        // SAFETY: inicializa COM en el hilo actual; se cierra en `Drop`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(dev)?;
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: equilibra el `CoInitializeEx` de `new` en el mismo hilo.
        unsafe { CoUninitialize() };
    }
}

/// Cliente de audio del dispositivo de salida por defecto y su formato.
struct Endpoint {
    client: IAudioClient,
    pwfx: *mut WAVEFORMATEX,
    format: DeviceFormat,
}

impl Endpoint {
    fn open_default_render() -> Result<Self, AudioError> {
        // SAFETY: llamadas COM documentadas; `pwfx` se libera en `Drop`.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(dev)?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| dev(format!("no hay dispositivo de salida de audio: {e}")))?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(dev)?;
            let pwfx = client.GetMixFormat().map_err(dev)?;
            let format = match parse_format(pwfx) {
                Ok(f) => f,
                Err(e) => {
                    CoTaskMemFree(Some(pwfx as *const _));
                    return Err(e);
                }
            };
            Ok(Self {
                client,
                pwfx,
                format,
            })
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // SAFETY: `pwfx` lo reservó `GetMixFormat` con CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(self.pwfx as *const _)) };
    }
}

/// Bytes del dispositivo → `f32` intercalado (en `out`).
fn device_to_f32(bytes: &[u8], kind: SampleKind, out: &mut Vec<f32>) {
    match kind {
        SampleKind::F32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        ),
        SampleKind::I16 => out.extend(
            bytes
                .chunks_exact(2)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0),
        ),
    }
}

/// `f32` intercalado → bytes del dispositivo.
fn f32_to_device(pcm: &[f32], kind: SampleKind, out: &mut [u8]) {
    match kind {
        SampleKind::F32 => {
            for (dst, v) in out.chunks_exact_mut(4).zip(pcm) {
                dst.copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleKind::I16 => {
            for (dst, v) in out.chunks_exact_mut(2).zip(pcm) {
                let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
                dst.copy_from_slice(&s.to_le_bytes());
            }
        }
    }
}

fn bytes_per_sample(kind: SampleKind) -> usize {
    match kind {
        SampleKind::F32 => 4,
        SampleKind::I16 => 2,
    }
}

/// Hilo de audio con parada cooperativa.
struct AudioThread {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Lanza `body` en un hilo con COM y espera a que informe si arrancó.
fn spawn_audio<F>(name: &str, body: F) -> Result<AudioThread, AudioError>
where
    F: FnOnce(Arc<AtomicBool>, mpsc::SyncSender<Result<(), AudioError>>) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let s = stop.clone();
    let join = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(s, ready_tx))
        .map_err(dev)?;
    let th = AudioThread {
        stop,
        join: Some(join),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(th),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(dev("el hilo de audio terminó al arrancar")),
    }
}

/// Captura del audio del sistema en tramas de 10 ms (48 kHz estéreo).
pub struct AudioCapture {
    frames: mpsc::Receiver<Vec<f32>>,
    _thread: AudioThread,
}

impl std::fmt::Debug for AudioCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioCapture")
    }
}

impl AudioCapture {
    /// Siguiente trama; `None` si no llegó ninguna en `timeout` (silencio:
    /// WASAPI no entrega paquetes cuando no suena nada) o si se cerró.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Vec<f32>> {
        self.frames.recv_timeout(timeout).ok()
    }
}

pub fn start_capture() -> Result<AudioCapture, AudioError> {
    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(32);
    let thread = spawn_audio("cs-audio-captura", move |stop, ready| {
        let run = || -> Result<(), AudioError> {
            let _com = ComGuard::new()?;
            let ep = Endpoint::open_default_render()?;
            let fmt = ep.format;
            // SAFETY: llamadas COM sobre un cliente recién activado; los
            // punteros de GetBuffer solo se leen hasta ReleaseBuffer.
            unsafe {
                ep.client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        200_000, // 20 ms en unidades de 100 ns
                        0,
                        ep.pwfx,
                        None,
                    )
                    .map_err(dev)?;
                let capture: IAudioCaptureClient = ep.client.GetService().map_err(dev)?;
                ep.client.Start().map_err(dev)?;
                let _ = ready.send(Ok(()));
                tracing::info!(
                    "captura de audio (loopback): {} Hz, {} canales, {:?}",
                    fmt.rate,
                    fmt.channels,
                    fmt.kind
                );
                let mut resampler = Resampler::new(fmt.rate, SAMPLE_RATE)?;
                let mut assembler = FrameAssembler::default();
                let (mut dev_pcm, mut stereo, mut at48) = (Vec::new(), Vec::new(), Vec::new());
                let frame_bytes = fmt.channels * bytes_per_sample(fmt.kind);
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                    loop {
                        let next = capture.GetNextPacketSize().map_err(dev)?;
                        if next == 0 {
                            break;
                        }
                        let mut data = std::ptr::null_mut();
                        let mut frames = 0u32;
                        let mut flags = 0u32;
                        capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                            .map_err(dev)?;
                        let n = frames as usize;
                        dev_pcm.clear();
                        if flags & BUFFERFLAGS_SILENT != 0 || data.is_null() {
                            dev_pcm.resize(n * fmt.channels, 0.0);
                        } else {
                            let bytes = std::slice::from_raw_parts(data, n * frame_bytes);
                            device_to_f32(bytes, fmt.kind, &mut dev_pcm);
                        }
                        capture.ReleaseBuffer(frames).map_err(dev)?;
                        stereo.clear();
                        to_stereo(&dev_pcm, fmt.channels, &mut stereo);
                        at48.clear();
                        resampler.process(&stereo, &mut at48);
                        assembler.push(&at48);
                        while let Some(frame) = assembler.next_frame() {
                            let _ = tx.try_send(frame);
                        }
                    }
                }
                let _ = ep.client.Stop();
            }
            Ok(())
        };
        if let Err(e) = run() {
            tracing::warn!("captura de audio: {e}");
            let _ = ready.send(Err(e));
        }
    })?;
    Ok(AudioCapture {
        frames: rx,
        _thread: thread,
    })
}

/// Reproducción desde un búfer de jitter compartido.
pub struct AudioPlayback {
    _thread: AudioThread,
}

impl std::fmt::Debug for AudioPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioPlayback")
    }
}

pub fn start_playback(jitter: Arc<Mutex<JitterBuffer>>) -> Result<AudioPlayback, AudioError> {
    let thread = spawn_audio("cs-audio-reproduccion", move |stop, ready| {
        let run = || -> Result<(), AudioError> {
            let _com = ComGuard::new()?;
            let ep = Endpoint::open_default_render()?;
            let fmt = ep.format;
            // SAFETY: llamadas COM sobre un cliente recién activado; el
            // puntero de GetBuffer solo se escribe dentro de `avail` tramas.
            unsafe {
                ep.client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        300_000, // 30 ms
                        0,
                        ep.pwfx,
                        None,
                    )
                    .map_err(dev)?;
                let event = CreateEventW(None, false, false, None).map_err(dev)?;
                let result = (|| -> Result<(), AudioError> {
                    ep.client.SetEventHandle(event).map_err(dev)?;
                    let render: IAudioRenderClient = ep.client.GetService().map_err(dev)?;
                    let buffer_frames = ep.client.GetBufferSize().map_err(dev)? as usize;
                    ep.client.Start().map_err(dev)?;
                    let _ = ready.send(Ok(()));
                    tracing::info!(
                        "reproducción de audio: {} Hz, {} canales, {:?}",
                        fmt.rate,
                        fmt.channels,
                        fmt.kind
                    );
                    let mut resampler = Resampler::new(SAMPLE_RATE, fmt.rate)?;
                    let bps = bytes_per_sample(fmt.kind);
                    // Muestras ya en el formato del dispositivo, listas para copiar.
                    let mut fifo: Vec<f32> = Vec::new();
                    let (mut block, mut res) = (vec![0.0f32; FRAME_LEN], Vec::new());
                    while !stop.load(Ordering::Relaxed) {
                        if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                            continue;
                        }
                        let padding = ep.client.GetCurrentPadding().map_err(dev)? as usize;
                        let avail = buffer_frames.saturating_sub(padding);
                        if avail == 0 {
                            continue;
                        }
                        while fifo.len() < avail * fmt.channels {
                            if let Ok(mut j) = jitter.lock() {
                                j.pull(&mut block);
                            }
                            res.clear();
                            resampler.process(&block, &mut res);
                            from_stereo(&res, fmt.channels, &mut fifo);
                        }
                        let data = render.GetBuffer(avail as u32).map_err(dev)?;
                        let out = std::slice::from_raw_parts_mut(data, avail * fmt.channels * bps);
                        f32_to_device(&fifo[..avail * fmt.channels], fmt.kind, out);
                        fifo.drain(..avail * fmt.channels);
                        render.ReleaseBuffer(avail as u32, 0).map_err(dev)?;
                    }
                    let _ = ep.client.Stop();
                    Ok(())
                })();
                let _ = CloseHandle(event);
                result
            }
        };
        if let Err(e) = run() {
            tracing::warn!("reproducción de audio: {e}");
            let _ = ready.send(Err(e));
        }
    })?;
    Ok(AudioPlayback { _thread: thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_conversion_roundtrip() {
        let pcm = [0.5f32, -0.25, 1.0, -1.0];
        for kind in [SampleKind::F32, SampleKind::I16] {
            let mut bytes = vec![0u8; pcm.len() * bytes_per_sample(kind)];
            f32_to_device(&pcm, kind, &mut bytes);
            let mut back = Vec::new();
            device_to_f32(&bytes, kind, &mut back);
            for (a, b) in pcm.iter().zip(back.iter()) {
                assert!((a - b).abs() < 1e-3, "{kind:?}: {a} vs {b}");
            }
        }
    }
}
