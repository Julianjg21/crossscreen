//! Audio del sistema para CrossScreen (ADR-0008).
//!
//! Formato interno fijo: **48 kHz, estéreo, `f32` intercalado**, tramas de
//! [`FRAME_MS`] ms ([`FRAME_SAMPLES`] muestras por canal).
//!
//! - [`codec`]: Opus con `opus1` (Rust puro).
//! - [`jitter`]: búfer de reproducción que absorbe la variación de llegada.
//! - [`resample`]: conversión de frecuencia y de canales para dispositivos que
//!   no trabajan a 48 kHz estéreo.

pub mod codec;
pub mod jitter;
pub mod resample;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use crate::windows::{start_capture, start_playback, AudioCapture, AudioPlayback};

#[cfg(all(not(windows), feature = "linux-pipewire"))]
mod linux;
#[cfg(all(not(windows), feature = "linux-pipewire"))]
pub use linux::{start_capture, start_playback, AudioCapture, AudioPlayback};

/// Frecuencia de muestreo interna.
pub const SAMPLE_RATE: u32 = 48_000;
/// Canales internos (estéreo intercalado).
pub const CHANNELS: usize = 2;
/// Duración de cada trama Opus.
pub const FRAME_MS: u32 = 10;
/// Muestras por canal en una trama.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
/// Valores `f32` intercalados en una trama.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("Opus: {0}")]
    Codec(String),
    #[error("dispositivo de audio: {0}")]
    Device(String),
    #[error("audio no soportado en esta plataforma todavía")]
    Unsupported,
}
