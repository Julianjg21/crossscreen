//! Decodificación de vídeo H.264 por hardware para `crossscreen-viewer`.
//!
//! Entrada: una *access unit* Annex-B por llamada (un `CompleteFrame.data`
//! del reensamblador de `cs-transport`). Salida: un frame en **memoria de
//! GPU** (textura NV12 de D3D11 + índice de subrecurso en Windows), listo para
//! que `cs-render` lo presente sin pasar por la CPU.
//!
//! Backends:
//! - **Windows:** MFT decodificador H.264 de Media Foundation
//!   (`CLSID_CMSH264DecoderMFT`) con aceleración DXVA sobre D3D11
//!   (`IMFDXGIDeviceManager` + `MFT_MESSAGE_SET_D3D_MANAGER`) y modo de baja
//!   latencia. Ver [`VideoDecoder`].
//! - **Linux (pendiente):** VAAPI con exportación DMA-BUF.
//!
//! El módulo [`annexb`] (partir un flujo en AUs) y [`stats`] (p50/p95) son
//! portables.

pub mod annexb;
pub mod stats;

#[cfg(windows)]
mod windows_mf;
#[cfg(windows)]
pub use windows_mf::{create_d3d11_device, DecodedFrame, VideoDecoder};

#[cfg(windows)]
pub mod test_encoder;

/// Errores del decodificador.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Una llamada del sistema (COM/Media Foundation/D3D11) falló.
    #[error("{call} falló: HRESULT 0x{code:08X}")]
    Os {
        /// Nombre de la llamada.
        call: &'static str,
        /// HRESULT devuelto.
        code: u32,
    },
    /// El sistema no ofrece lo necesario (p. ej. sin aceleración D3D11).
    #[error("no soportado: {0}")]
    Unsupported(String),
    /// El decodificador produjo algo inesperado.
    #[error("salida inesperada del decodificador: {0}")]
    Unexpected(String),
}

/// Resultado de este crate.
pub type Result<T> = std::result::Result<T, DecodeError>;
