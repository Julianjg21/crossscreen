//! Lo único del visor que depende de la plataforma: el contexto de vídeo que
//! comparten decodificador y renderizador, la creación del renderizador sobre
//! la ventana y la vista del frame decodificado que consume el renderizador.
//!
//! - **Windows:** dispositivo D3D11 compartido; el frame es una textura NV12
//!   en GPU (sin copias).
//! - **Linux:** OpenH264 por software; el frame son planos I420 en CPU que
//!   `wgpu` sube a la GPU.

use anyhow::Result;
use cs_decode::DecodedFrame;
use cs_render::{FrameRef, Renderer, RendererOptions};
use std::sync::Arc;
use winit::window::Window;

/// Contexto compartido por el decodificador (hilo propio) y el renderizador.
#[cfg(windows)]
pub type VideoContext = windows::Win32::Graphics::Direct3D11::ID3D11Device;
#[cfg(not(windows))]
pub type VideoContext = cs_decode::DecoderContext;

#[cfg(windows)]
pub fn create_video_context() -> Result<VideoContext> {
    use anyhow::Context;
    cs_decode::create_d3d11_device().context("creando el dispositivo D3D11")
}

#[cfg(not(windows))]
pub fn create_video_context() -> Result<VideoContext> {
    use anyhow::Context;
    cs_decode::create_decoder_context().context("preparando el decodificador OpenH264")
}

/// Crea el renderizador sobre la ventana de la sesión.
#[cfg(windows)]
pub fn create_renderer(
    ctx: &VideoContext,
    window: &Arc<Window>,
    width: u32,
    height: u32,
    options: RendererOptions,
) -> Result<Renderer> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let hwnd = match window.window_handle()?.as_raw() {
        RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
        other => anyhow::bail!("handle de ventana inesperado: {other:?}"),
    };
    Ok(Renderer::new(ctx, hwnd, width, height, options)?)
}

#[cfg(not(windows))]
pub fn create_renderer(
    _ctx: &VideoContext,
    window: &Arc<Window>,
    width: u32,
    height: u32,
    options: RendererOptions,
) -> Result<Renderer> {
    let r = Renderer::new(window.clone(), width, height, options)?;
    cs_log::out!("renderizador: {}", r.adapter_info());
    Ok(r)
}

/// Vista del frame decodificado para el renderizador.
#[cfg(windows)]
pub fn frame_ref(f: &DecodedFrame) -> FrameRef<'_> {
    FrameRef {
        texture: &f.texture,
        subresource: f.subresource,
        width: f.width,
        height: f.height,
    }
}

#[cfg(not(windows))]
pub fn frame_ref(f: &DecodedFrame) -> FrameRef<'_> {
    FrameRef {
        y: &f.y,
        u: &f.u,
        v: &f.v,
        strides: f.strides,
        width: f.width,
        height: f.height,
    }
}
