//! Presentación de vídeo decodificado para `crossscreen-viewer`.
//!
//! **Decisión de la PoC-5 (Windows):** se presenta con **D3D11 + cadena DXGI
//! en modo *flip*** sobre el **mismo dispositivo** que usa el decodificador de
//! `cs-decode`. La textura NV12 del MFT se muestrea directamente desde un
//! *pixel shader* (dos SRV: luma `R8_UNORM` y croma `R8G8_UNORM`) que
//! convierte a RGB: **cero copias**, ni a CPU ni entre texturas de GPU.
//!
//! Por qué no `wgpu` aquí: `wgpu` (backend DX12) no puede importar
//! texturas D3D11 NV12 sin pasar por un *shared handle* + `OpenSharedHandle`
//! en D3D12 + `texture_from_raw` de wgpu-hal, y el soporte de NV12 multiplanar
//! en wgpu es experimental. La ventana sí se crea con `winit` (en el visor),
//! y `wgpu` queda como camino multiplataforma (Linux: VAAPI → DMA-BUF →
//! Vulkan).
//!
//! El cursor llega fuera de banda: [`Renderer::set_cursor`] lo dibuja como un
//! *quad* superpuesto, sin tocar el frame de vídeo. Con
//! [`Renderer::set_cursor_shape`] se sube la imagen real del cursor (RGBA de
//! la fuente) a una textura; hasta entonces se dibuja una flecha procedimental.

#[cfg(windows)]
mod d3d11;
#[cfg(windows)]
pub use d3d11::{FrameRef, PresentStats, Renderer, RendererOptions};

// Linux y otras plataformas: wgpu (Vulkan/GL), frames I420 desde la CPU.
#[cfg(not(windows))]
mod wgpu_backend;
#[cfg(not(windows))]
pub use wgpu_backend::{FrameRef, PresentStats, Renderer, RendererOptions};

/// Errores del renderizador.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// Una llamada del sistema (D3D11/DXGI) falló.
    #[error("{call} falló: HRESULT 0x{code:08X}")]
    Os {
        /// Nombre de la llamada.
        call: &'static str,
        /// HRESULT devuelto.
        code: u32,
    },
    /// Error al compilar los shaders.
    #[error("compilación de shader: {0}")]
    Shader(String),
    /// Otro error.
    #[error("{0}")]
    Other(String),
}

/// Resultado de este crate.
pub type Result<T> = std::result::Result<T, RenderError>;

/// Rectángulo de destino del vídeo (en píxeles de la ventana) que conserva la
/// relación de aspecto (*letterbox*/*pillarbox*). Devuelve `(x, y, ancho, alto)`.
pub fn fit_rect(video_w: u32, video_h: u32, win_w: u32, win_h: u32) -> (f32, f32, f32, f32) {
    if video_w == 0 || video_h == 0 || win_w == 0 || win_h == 0 {
        return (0.0, 0.0, win_w as f32, win_h as f32);
    }
    let (vw, vh, ww, wh) = (video_w as f32, video_h as f32, win_w as f32, win_h as f32);
    let scale = (ww / vw).min(wh / vh);
    let (w, h) = (vw * scale, vh * scale);
    ((ww - w) / 2.0, (wh - h) / 2.0, w, h)
}

/// Rectángulo del cursor en píxeles de la ventana, `(x, y, ancho, alto)`.
///
/// - `pos`: punto caliente en píxeles del vídeo (monitor remoto).
/// - `hotspot`, `size`: de la forma del cursor, en píxeles del vídeo.
/// - `video_rect`: lo que devuelve [`fit_rect`] para el vídeo actual.
/// - `video_w`: ancho del vídeo en píxeles; el cursor se escala igual que él.
pub fn cursor_rect(
    pos: (f32, f32),
    hotspot: (f32, f32),
    size: (f32, f32),
    video_rect: (f32, f32, f32, f32),
    video_w: u32,
) -> (f32, f32, f32, f32) {
    let (x, y, w, _) = video_rect;
    let s = if video_w > 0 { w / video_w as f32 } else { 1.0 };
    (
        x + (pos.0 - hotspot.0) * s,
        y + (pos.1 - hotspot.1) * s,
        size.0 * s,
        size.1 * s,
    )
}

/// Convierte RGBA8 no premultiplicado (como llega en `CursorShape`) a
/// premultiplicado, para filtrar y mezclar sin halos al escalar.
pub fn premultiply_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let a = u16::from(px[3]);
        let m = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
        out.extend_from_slice(&[m(px[0]), m(px[1]), m(px[2]), px[3]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{cursor_rect, fit_rect, premultiply_rgba};

    #[test]
    fn cursor_rect_scales_with_video() {
        // 1:1: el punto caliente cae en `pos`.
        let r = cursor_rect(
            (100.0, 50.0),
            (4.0, 2.0),
            (32.0, 32.0),
            (0.0, 0.0, 1920.0, 1080.0),
            1920,
        );
        assert_eq!(r, (96.0, 48.0, 32.0, 32.0));
        // Vídeo 1920 presentado a 960 con bandas verticales de 270.
        let r = cursor_rect(
            (100.0, 50.0),
            (4.0, 2.0),
            (32.0, 32.0),
            (0.0, 270.0, 960.0, 540.0),
            1920,
        );
        assert_eq!(r, (48.0, 294.0, 16.0, 16.0));
    }

    #[test]
    fn premultiply() {
        assert_eq!(
            premultiply_rgba(&[255, 128, 0, 255, 255, 255, 255, 0, 200, 100, 50, 128]),
            vec![255, 128, 0, 255, 0, 0, 0, 0, 100, 50, 25, 128]
        );
    }

    #[test]
    fn fit_rect_letterbox_and_pillarbox() {
        assert_eq!(fit_rect(1920, 1080, 1920, 1080), (0.0, 0.0, 1920.0, 1080.0));
        // Ventana más alta: bandas arriba y abajo.
        assert_eq!(fit_rect(1920, 1080, 960, 1080), (0.0, 270.0, 960.0, 540.0));
        // Ventana más ancha: bandas a los lados.
        assert_eq!(fit_rect(1280, 720, 1600, 720), (160.0, 0.0, 1280.0, 720.0));
        assert_eq!(fit_rect(0, 0, 100, 50), (0.0, 0.0, 100.0, 50.0));
    }
}
