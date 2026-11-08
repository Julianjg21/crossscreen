//! Presentación con D3D11 + DXGI *flip model* (Windows).
//!
//! - Cadena `DXGI_SWAP_EFFECT_FLIP_DISCARD`, 2 búferes, BGRA8, con
//!   `FRAME_LATENCY_WAITABLE_OBJECT` y `SetMaximumFrameLatency(1)`: la CPU no
//!   encola más de un frame por delante del *scan-out*.
//! - NV12 → RGB en *pixel shader* leyendo la textura del decodificador por
//!   SRV (cero copias). Si el driver no permite SRV sobre la superficie del
//!   decodificador, se cae a una copia GPU→GPU (`CopySubresourceRegion`) a
//!   una textura NV12 propia; nunca se pasa por la CPU.
//! - Latencia de presentación medida con `GetFrameStatistics`: QPC del
//!   *vblank* en que se mostró el frame N menos QPC de la llamada a
//!   `Present` del frame N.
#![allow(unsafe_code)] // FFI D3D11/DXGI; cada bloque unsafe está acotado y comentado.

use crate::{cursor_rect, fit_rect, premultiply_rgba, RenderError, Result};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::WaitForSingleObjectEx;

const SHADER_SRC: &str = include_str!("shaders.hlsl");
const BUFFER_COUNT: u32 = 2;

fn os<T>(r: windows::core::Result<T>, call: &'static str) -> Result<T> {
    r.map_err(|e| RenderError::Os {
        call,
        code: e.code().0 as u32,
    })
}

/// Referencia a un frame NV12 en GPU (p. ej. un `cs_decode::DecodedFrame`).
#[derive(Debug, Clone, Copy)]
pub struct FrameRef<'a> {
    /// Textura NV12 (puede ser un array de superficies del decodificador).
    pub texture: &'a ID3D11Texture2D,
    /// Índice de subrecurso / rebanada del array.
    pub subresource: u32,
    /// Ancho visible.
    pub width: u32,
    /// Alto visible.
    pub height: u32,
}

/// Opciones de creación.
#[derive(Debug, Clone, Copy)]
pub struct RendererOptions {
    /// `true`: `Present(1)` sincronizado con el *vblank*. `false`:
    /// `Present(0, ALLOW_TEARING)` si el sistema lo soporta.
    pub vsync: bool,
}

impl Default for RendererOptions {
    fn default() -> Self {
        Self { vsync: true }
    }
}

/// Tiempos de un frame presentado.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresentStats {
    /// CPU: dibujar (vídeo + cursor) y llamar a `Present`.
    pub cpu_submit: Duration,
    /// Latencia `Present` → *vblank* en que se mostró, de un frame anterior
    /// cuya estadística acaba de estar disponible (DXGI la entrega con
    /// retraso). `None` si DXGI no la ofrece.
    pub present_to_vblank: Option<Duration>,
}

#[derive(Debug)]
struct Params {
    video_rect: [f32; 4],
    uv_scale: [f32; 2],
    _pad: [f32; 2],
    cursor_rect: [f32; 4],
}

impl Params {
    fn as_bytes(&self) -> [u8; 48] {
        let mut out = [0u8; 48];
        let vals = [
            self.video_rect[0],
            self.video_rect[1],
            self.video_rect[2],
            self.video_rect[3],
            self.uv_scale[0],
            self.uv_scale[1],
            self._pad[0],
            self._pad[1],
            self.cursor_rect[0],
            self.cursor_rect[1],
            self.cursor_rect[2],
            self.cursor_rect[3],
        ];
        for (i, v) in vals.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Forma real del cursor subida a la GPU.
#[derive(Debug)]
struct CursorTexture {
    srv: ID3D11ShaderResourceView,
    size: (f32, f32),
    hotspot: (f32, f32),
}

#[derive(Debug)]
struct SrvPair {
    texture: ID3D11Texture2D,
    subresource: u32,
    y: ID3D11ShaderResourceView,
    uv: ID3D11ShaderResourceView,
}

/// Presentador D3D11 de una ventana.
#[derive(Debug)]
pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain2,
    waitable: HANDLE,
    rtv: Option<ID3D11RenderTargetView>,
    vs_video: ID3D11VertexShader,
    vs_cursor: ID3D11VertexShader,
    ps_video: ID3D11PixelShader,
    ps_cursor: ID3D11PixelShader,
    ps_cursor_tex: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend_alpha: ID3D11BlendState,
    blend_premul: ID3D11BlendState,
    cbuffer: ID3D11Buffer,
    query: ID3D11Query,
    width: u32,
    height: u32,
    tearing: bool,
    opts: RendererOptions,
    srv_cache: Vec<SrvPair>,
    /// Textura propia para el camino de respaldo (copia GPU→GPU).
    fallback_tex: Option<ID3D11Texture2D>,
    zero_copy: Option<bool>,
    cursor_pos: (f32, f32),
    cursor_visible: bool,
    cursor_size: (f32, f32),
    cursor_tex: Option<CursorTexture>,
    last_video: (u32, u32),
    /// (PresentCount, QPC en la llamada a Present).
    present_qpc: VecDeque<(u32, i64)>,
    qpc_freq: i64,
    last_stats_count: u32,
    /// QPC del último *vblank* conocido (`DXGI_FRAME_STATISTICS::SyncQPCTime`).
    last_sync_qpc: Option<i64>,
}

impl Renderer {
    /// Crea la cadena de intercambio para `hwnd` sobre `device` (el mismo que
    /// el decodificador). `width`×`height` es el tamaño del área cliente.
    pub fn new(
        device: &ID3D11Device,
        hwnd: HWND,
        width: u32,
        height: u32,
        opts: RendererOptions,
    ) -> Result<Self> {
        // SAFETY: todas las llamadas reciben interfaces válidas y punteros a
        // estructuras locales; `hwnd` lo aporta el llamador y es una ventana viva.
        unsafe {
            let context = os(device.GetImmediateContext(), "GetImmediateContext")?;
            let dxgi_device: IDXGIDevice = os(device.cast(), "QueryInterface(IDXGIDevice)")?;
            let adapter = os(dxgi_device.GetAdapter(), "IDXGIDevice::GetAdapter")?;
            let factory: IDXGIFactory2 = os(adapter.GetParent(), "IDXGIAdapter::GetParent")?;

            let tearing = match factory.cast::<IDXGIFactory5>() {
                Ok(f5) => {
                    let mut allow = BOOL(0);
                    f5.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut allow as *mut BOOL as *mut _,
                        std::mem::size_of::<BOOL>() as u32,
                    )
                    .is_ok()
                        && allow.as_bool()
                }
                Err(_) => false,
            };

            let flags = swapchain_flags(tearing);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: BOOL(0),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFER_COUNT,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags.0 as u32,
            };
            let sc1 = os(
                factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None),
                "CreateSwapChainForHwnd",
            )?;
            // Alt+Enter (pantalla completa exclusiva) no: usamos ventana sin
            // bordes, que con flip model obtiene *independent flip*.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let swapchain: IDXGISwapChain2 = os(sc1.cast(), "QueryInterface(IDXGISwapChain2)")?;
            os(
                swapchain.SetMaximumFrameLatency(1),
                "SetMaximumFrameLatency",
            )?;
            let waitable = swapchain.GetFrameLatencyWaitableObject();

            let vs_blob = compile("vs_video", "vs_4_0")?;
            let vs_cursor_blob = compile("vs_cursor", "vs_4_0")?;
            let ps_blob = compile("ps_video", "ps_4_0")?;
            let ps_cursor_blob = compile("ps_cursor", "ps_4_0")?;
            let ps_cursor_tex_blob = compile("ps_cursor_tex", "ps_4_0")?;
            let mut vs_video = None;
            os(
                device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs_video)),
                "CreateVertexShader",
            )?;
            let mut vs_cursor = None;
            os(
                device.CreateVertexShader(blob_bytes(&vs_cursor_blob), None, Some(&mut vs_cursor)),
                "CreateVertexShader",
            )?;
            let mut ps_video = None;
            os(
                device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps_video)),
                "CreatePixelShader",
            )?;
            let mut ps_cursor = None;
            os(
                device.CreatePixelShader(blob_bytes(&ps_cursor_blob), None, Some(&mut ps_cursor)),
                "CreatePixelShader",
            )?;
            let mut ps_cursor_tex = None;
            os(
                device.CreatePixelShader(
                    blob_bytes(&ps_cursor_tex_blob),
                    None,
                    Some(&mut ps_cursor_tex),
                ),
                "CreatePixelShader",
            )?;

            let mut sampler = None;
            os(
                device.CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MipLODBias: 0.0,
                        MaxAnisotropy: 1,
                        ComparisonFunc: D3D11_COMPARISON_NEVER,
                        BorderColor: [0.0; 4],
                        MinLOD: 0.0,
                        MaxLOD: f32::MAX,
                    },
                    Some(&mut sampler),
                ),
                "CreateSamplerState",
            )?;

            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: BOOL(1),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ZERO,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend_alpha = None;
            os(
                device.CreateBlendState(&blend_desc, Some(&mut blend_alpha)),
                "CreateBlendState",
            )?;
            // Alfa premultiplicado para la textura del cursor real.
            blend_desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
            let mut blend_premul = None;
            os(
                device.CreateBlendState(&blend_desc, Some(&mut blend_premul)),
                "CreateBlendState(premul)",
            )?;

            let mut cbuffer = None;
            os(
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 48,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut cbuffer),
                ),
                "CreateBuffer(cbuffer)",
            )?;

            let mut query = None;
            os(
                device.CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                ),
                "CreateQuery(EVENT)",
            )?;

            let mut freq = 0i64;
            let _ = QueryPerformanceFrequency(&mut freq);

            let missing = || RenderError::Other("D3D11 devolvió un objeto nulo".into());
            let mut r = Self {
                device: device.clone(),
                context,
                swapchain,
                waitable,
                rtv: None,
                vs_video: vs_video.ok_or_else(missing)?,
                vs_cursor: vs_cursor.ok_or_else(missing)?,
                ps_video: ps_video.ok_or_else(missing)?,
                ps_cursor: ps_cursor.ok_or_else(missing)?,
                ps_cursor_tex: ps_cursor_tex.ok_or_else(missing)?,
                sampler: sampler.ok_or_else(missing)?,
                blend_alpha: blend_alpha.ok_or_else(missing)?,
                blend_premul: blend_premul.ok_or_else(missing)?,
                cbuffer: cbuffer.ok_or_else(missing)?,
                query: query.ok_or_else(missing)?,
                width: width.max(1),
                height: height.max(1),
                tearing,
                opts,
                srv_cache: Vec::new(),
                fallback_tex: None,
                zero_copy: None,
                cursor_pos: (0.0, 0.0),
                cursor_visible: false,
                cursor_size: (20.0, 28.0),
                cursor_tex: None,
                last_video: (0, 0),
                present_qpc: VecDeque::new(),
                qpc_freq: freq.max(1),
                last_stats_count: 0,
                last_sync_qpc: None,
            };
            r.create_rtv()?;
            tracing::info!(
                "renderer D3D11: flip-discard {}x{}, tearing={}, vsync={}",
                r.width,
                r.height,
                tearing,
                opts.vsync
            );
            Ok(r)
        }
    }

    /// `Some(true)` si el último frame se muestreó directamente de la
    /// superficie del decodificador; `Some(false)` si hizo falta la copia
    /// GPU→GPU de respaldo; `None` antes del primer frame.
    pub fn zero_copy(&self) -> Option<bool> {
        self.zero_copy
    }

    /// Tamaño actual del *back buffer*.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Redimensiona los búferes al nuevo tamaño del área cliente.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let (w, h) = (width.max(1), height.max(1));
        if (w, h) == (self.width, self.height) {
            return Ok(());
        }
        self.rtv = None;
        // SAFETY: no quedan referencias a los búferes (rtv soltado y
        // desvinculado del pipeline).
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            os(
                self.swapchain.ResizeBuffers(
                    BUFFER_COUNT,
                    w,
                    h,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    swapchain_flags(self.tearing),
                ),
                "ResizeBuffers",
            )?;
        }
        self.width = w;
        self.height = h;
        self.create_rtv()
    }

    /// Posición del cursor en píxeles del **vídeo** (coordenadas del
    /// escritorio remoto) y visibilidad. Se dibuja en el siguiente `render`.
    pub fn set_cursor(&mut self, pos: (f32, f32), visible: bool) {
        self.cursor_pos = pos;
        self.cursor_visible = visible;
    }

    /// Sube la imagen real del cursor: RGBA8 **no premultiplicado**, fila a
    /// fila sin relleno (`width * height * 4` bytes), con el punto caliente
    /// en `hotspot`. Sustituye a la flecha procedimental desde el siguiente
    /// `render`. La textura es inmutable: cada cambio de forma crea una nueva
    /// (los cambios de forma son raros; las posiciones van por `set_cursor`).
    pub fn set_cursor_shape(
        &mut self,
        width: u32,
        height: u32,
        hotspot: (u32, u32),
        rgba: &[u8],
    ) -> Result<()> {
        if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
            return Err(RenderError::Other(format!(
                "forma de cursor inválida: {width}x{height}, {} bytes",
                rgba.len()
            )));
        }
        let pixels = premultiply_rgba(rgba);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex = None;
        let mut srv = None;
        // SAFETY: `pixels` tiene exactamente `height` filas de `width * 4`
        // bytes y vive durante la llamada; descripciones locales válidas.
        unsafe {
            os(
                self.device
                    .CreateTexture2D(&desc, Some(&init), Some(&mut tex)),
                "CreateTexture2D(cursor)",
            )?;
            let tex = tex.ok_or_else(|| RenderError::Other("textura de cursor nula".into()))?;
            os(
                self.device
                    .CreateShaderResourceView(&tex, None, Some(&mut srv)),
                "CreateShaderResourceView(cursor)",
            )?;
        }
        self.cursor_tex = Some(CursorTexture {
            srv: srv.ok_or_else(|| RenderError::Other("SRV de cursor nula".into()))?,
            size: (width as f32, height as f32),
            hotspot: (hotspot.0 as f32, hotspot.1 as f32),
        });
        Ok(())
    }

    /// `true` si ya se subió una forma real con [`Renderer::set_cursor_shape`].
    pub fn has_cursor_shape(&self) -> bool {
        self.cursor_tex.is_some()
    }

    /// Espera (como mucho `timeout`) a que la cadena admita un frame nuevo.
    /// Llamarlo antes de `render` minimiza la latencia de cola. Devuelve
    /// `true` si se señalizó a tiempo.
    pub fn wait_for_frame(&self, timeout: Duration) -> bool {
        // SAFETY: handle propiedad de la cadena, válido mientras viva.
        let r =
            unsafe { WaitForSingleObjectEx(self.waitable, timeout.as_millis() as u32, BOOL(1)) };
        r == WAIT_OBJECT_0
    }

    /// Dibuja `frame` (o negro si es `None`) y el cursor en el *back buffer*.
    /// No presenta: llamar después a [`Renderer::present`] (entre medias se
    /// puede capturar con [`Renderer::capture_rgba`]).
    pub fn render(&mut self, frame: Option<FrameRef<'_>>) -> Result<()> {
        let rtv = self
            .rtv
            .clone()
            .ok_or_else(|| RenderError::Other("sin render target".into()))?;
        let views = match frame {
            Some(f) => Some(self.views_for(f)?),
            None => None,
        };
        if let Some(f) = frame {
            self.last_video = (f.width, f.height);
        }
        let (vw, vh) = self.last_video;
        let (x, y, w, h) = fit_rect(vw, vh, self.width, self.height);
        let to_ndc = |px: f32, py: f32| -> (f32, f32) {
            (
                px / self.width as f32 * 2.0 - 1.0,
                1.0 - py / self.height as f32 * 2.0,
            )
        };
        let (x0, y0) = to_ndc(x, y);
        let (x1, y1) = to_ndc(x + w, y + h);
        let uv_scale = match (frame, views.as_ref()) {
            (Some(f), Some((_, _, tw, th))) => {
                [f.width as f32 / *tw as f32, f.height as f32 / *th as f32]
            }
            _ => [1.0, 1.0],
        };
        // Cursor: posición en píxeles de vídeo → píxeles de ventana. La forma
        // real se escala como el vídeo; la flecha de respaldo tiene tamaño fijo.
        let (cx, cy, cw, ch) = match &self.cursor_tex {
            Some(c) => cursor_rect(self.cursor_pos, c.hotspot, c.size, (x, y, w, h), vw),
            None => {
                let (px, py, _, _) =
                    cursor_rect(self.cursor_pos, (0.0, 0.0), (0.0, 0.0), (x, y, w, h), vw);
                (px, py, self.cursor_size.0, self.cursor_size.1)
            }
        };
        let (c0x, c0y) = to_ndc(cx, cy);
        let (c1x, c1y) = to_ndc(cx + cw, cy + ch);
        let params = Params {
            video_rect: [x0, y0, x1, y1],
            uv_scale,
            _pad: [0.0; 2],
            cursor_rect: [c0x, c0y, c1x, c1y],
        };
        let bytes = params.as_bytes();
        // SAFETY: todos los objetos pertenecen a `self.device`; `bytes` vive
        // durante la llamada a UpdateSubresource.
        unsafe {
            let ctx = &self.context;
            ctx.UpdateSubresource(&self.cbuffer, 0, None, bytes.as_ptr() as *const _, 0, 0);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            if let Some((ysrv, uvsrv, _, _)) = views {
                ctx.OMSetBlendState(None, None, 0xffff_ffff);
                ctx.VSSetShader(&self.vs_video, None);
                ctx.PSSetShader(&self.ps_video, None);
                ctx.PSSetShaderResources(0, Some(&[Some(ysrv), Some(uvsrv)]));
                ctx.Draw(4, 0);
                ctx.PSSetShaderResources(0, Some(&[None, None]));
            }
            if self.cursor_visible {
                ctx.VSSetShader(&self.vs_cursor, None);
                match &self.cursor_tex {
                    Some(c) => {
                        ctx.OMSetBlendState(&self.blend_premul, None, 0xffff_ffff);
                        ctx.PSSetShader(&self.ps_cursor_tex, None);
                        ctx.PSSetShaderResources(2, Some(&[Some(c.srv.clone())]));
                        ctx.Draw(4, 0);
                        ctx.PSSetShaderResources(2, Some(&[None]));
                    }
                    None => {
                        ctx.OMSetBlendState(&self.blend_alpha, None, 0xffff_ffff);
                        ctx.PSSetShader(&self.ps_cursor, None);
                        ctx.Draw(4, 0);
                    }
                }
            }
        }
        Ok(())
    }

    /// Espera a que la GPU termine todo lo encolado (p. ej. el `render`
    /// recién emitido). Solo para medir: serializa CPU y GPU.
    pub fn wait_gpu(&self) {
        // SAFETY: query y contexto válidos; `done` es el BOOL que espera
        // D3D11_QUERY_EVENT.
        unsafe {
            self.context.End(&self.query);
            loop {
                let mut done = BOOL(0);
                let r = self.context.GetData(
                    &self.query,
                    Some(&mut done as *mut BOOL as *mut _),
                    std::mem::size_of::<BOOL>() as u32,
                    0,
                );
                if r.is_err() || done.as_bool() {
                    break;
                }
                std::hint::spin_loop();
            }
        }
    }

    /// Presenta el *back buffer*. Devuelve los tiempos medidos.
    pub fn present(&mut self) -> Result<PresentStats> {
        let t0 = Instant::now();
        let (interval, flags) = if self.opts.vsync {
            (1, DXGI_PRESENT(0))
        } else if self.tearing {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (0, DXGI_PRESENT(0))
        };
        let mut qpc = 0i64;
        // SAFETY: cadena válida; QueryPerformanceCounter escribe en un i64 local.
        let hr = unsafe {
            let _ = QueryPerformanceCounter(&mut qpc);
            self.swapchain.Present(interval, flags)
        };
        if hr.is_err() {
            return Err(RenderError::Os {
                call: "IDXGISwapChain::Present",
                code: hr.0 as u32,
            });
        }
        let cpu_present = t0.elapsed();
        // SAFETY: consultas de estadísticas sobre la cadena válida.
        let present_to_vblank = unsafe {
            if let Ok(count) = self.swapchain.GetLastPresentCount() {
                self.present_qpc.push_back((count, qpc));
                while self.present_qpc.len() > 16 {
                    self.present_qpc.pop_front();
                }
            }
            let mut st = DXGI_FRAME_STATISTICS::default();
            if self.swapchain.GetFrameStatistics(&mut st).is_ok()
                && st.PresentCount != self.last_stats_count
            {
                self.last_stats_count = st.PresentCount;
                if st.SyncQPCTime > 0 {
                    self.last_sync_qpc = Some(st.SyncQPCTime);
                }
                self.present_qpc
                    .iter()
                    .find(|(c, _)| *c == st.PresentCount)
                    .and_then(|&(_, q)| {
                        let d = st.SyncQPCTime - q;
                        (d >= 0).then(|| {
                            Duration::from_nanos(
                                (d as u128 * 1_000_000_000 / self.qpc_freq as u128) as u64,
                            )
                        })
                    })
            } else {
                None
            }
        };
        Ok(PresentStats {
            cpu_submit: cpu_present,
            present_to_vblank,
        })
    }

    /// Tiempo estimado hasta el siguiente *vblank*, extrapolando el último
    /// conocido de las estadísticas de DXGI con el periodo de refresco
    /// `period`. `None` hasta que DXGI entregue la primera estadística.
    /// Sirve para retrasar los repintados que solo mueven el cursor hasta
    /// justo antes del *vblank* (*late latching*).
    pub fn time_to_next_vblank(&self, period: Duration) -> Option<Duration> {
        let sync = self.last_sync_qpc?;
        let mut now = 0i64;
        // SAFETY: escribe en un i64 local.
        let _ = unsafe { QueryPerformanceCounter(&mut now) };
        let period_qpc = (period.as_secs_f64() * self.qpc_freq as f64) as i64;
        if period_qpc <= 0 || now < sync {
            return None;
        }
        let left = period_qpc - (now - sync) % period_qpc;
        Some(Duration::from_secs_f64(left as f64 / self.qpc_freq as f64))
    }

    /// Copia el *back buffer* actual (antes de `present`) a CPU en RGBA8.
    /// Solo para verificación/diagnóstico: hace una copia a una textura
    /// *staging* y espera a la GPU.
    pub fn capture_rgba(&self) -> Result<(u32, u32, Vec<u8>)> {
        // SAFETY: la textura staging tiene el mismo tamaño y formato que el
        // back buffer; se mapea en lectura y se copia fila a fila respetando
        // RowPitch; se desmapea antes de salir.
        unsafe {
            let back: ID3D11Texture2D = os(self.swapchain.GetBuffer(0), "GetBuffer")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            os(
                self.device.CreateTexture2D(&desc, None, Some(&mut staging)),
                "CreateTexture2D(staging)",
            )?;
            let staging = staging.ok_or_else(|| RenderError::Other("staging nula".into()))?;
            self.context.CopyResource(&staging, &back);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            os(
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)),
                "Map(staging)",
            )?;
            let (w, h) = (desc.Width as usize, desc.Height as usize);
            let mut out = vec![0u8; w * h * 4];
            for row in 0..h {
                let src = std::slice::from_raw_parts(
                    (map.pData as *const u8).add(row * map.RowPitch as usize),
                    w * 4,
                );
                let dst = &mut out[row * w * 4..(row + 1) * w * 4];
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&[s[2], s[1], s[0], 255]);
                }
            }
            self.context.Unmap(&staging, 0);
            Ok((desc.Width, desc.Height, out))
        }
    }

    fn create_rtv(&mut self) -> Result<()> {
        // SAFETY: back buffer 0 de la cadena; la vista se guarda en `self`.
        unsafe {
            let back: ID3D11Texture2D = os(self.swapchain.GetBuffer(0), "GetBuffer")?;
            let mut rtv = None;
            os(
                self.device
                    .CreateRenderTargetView(&back, None, Some(&mut rtv)),
                "CreateRenderTargetView",
            )?;
            self.rtv = rtv;
        }
        Ok(())
    }

    /// SRV de luma y croma para la rebanada `subresource` de la textura NV12
    /// (cacheadas), más el tamaño de la superficie.
    fn views_for(
        &mut self,
        f: FrameRef<'_>,
    ) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView, u32, u32)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: textura válida aportada por el llamador.
        unsafe { f.texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(RenderError::Other(format!(
                "formato no soportado: {:?}",
                desc.Format
            )));
        }
        if self.zero_copy != Some(false) {
            if let Some(p) = self
                .srv_cache
                .iter()
                .find(|p| p.texture == *f.texture && p.subresource == f.subresource)
            {
                return Ok((p.y.clone(), p.uv.clone(), desc.Width, desc.Height));
            }
            if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
                match self.create_srvs(f.texture, f.subresource) {
                    Ok((y, uv)) => {
                        if self.zero_copy.is_none() {
                            tracing::info!(
                                "zero-copy: SRV directa sobre la superficie del decodificador ({}x{}, array {})",
                                desc.Width,
                                desc.Height,
                                desc.ArraySize
                            );
                        }
                        self.zero_copy = Some(true);
                        // Si cambia la textura (cambio de resolución), purgar.
                        self.srv_cache.retain(|p| p.texture == *f.texture);
                        self.srv_cache.push(SrvPair {
                            texture: f.texture.clone(),
                            subresource: f.subresource,
                            y: y.clone(),
                            uv: uv.clone(),
                        });
                        return Ok((y, uv, desc.Width, desc.Height));
                    }
                    Err(e) => tracing::warn!(
                        "SRV sobre la superficie del decodificador falló ({e}); copia GPU"
                    ),
                }
            } else {
                tracing::warn!(
                    "la superficie del decodificador no tiene BIND_SHADER_RESOURCE; copia GPU"
                );
            }
        }
        // Respaldo: copia GPU→GPU a una NV12 propia muestreable.
        self.zero_copy = Some(false);
        let need_new = match &self.fallback_tex {
            Some(t) => {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: textura propia válida.
                unsafe { t.GetDesc(&mut d) };
                d.Width != desc.Width || d.Height != desc.Height
            }
            None => true,
        };
        if need_new {
            let nd = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut t = None;
            // SAFETY: descripción local válida.
            os(
                unsafe { self.device.CreateTexture2D(&nd, None, Some(&mut t)) },
                "CreateTexture2D(NV12)",
            )?;
            self.fallback_tex = t;
            self.srv_cache.clear();
        }
        let tex = self
            .fallback_tex
            .clone()
            .ok_or_else(|| RenderError::Other("sin textura".into()))?;
        // SAFETY: mismo formato y tamaño; subrecurso de origen válido.
        unsafe {
            self.context
                .CopySubresourceRegion(&tex, 0, 0, 0, 0, f.texture, f.subresource, None)
        };
        if self.srv_cache.is_empty() {
            let (y, uv) = self.create_srvs(&tex, 0)?;
            self.srv_cache.push(SrvPair {
                texture: tex.clone(),
                subresource: 0,
                y,
                uv,
            });
        }
        let p = &self.srv_cache[0];
        Ok((p.y.clone(), p.uv.clone(), desc.Width, desc.Height))
    }

    fn create_srvs(
        &self,
        tex: &ID3D11Texture2D,
        slice: u32,
    ) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView)> {
        let make = |format: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
            };
            let mut srv = None;
            // SAFETY: textura NV12 válida; R8 = plano Y, R8G8 = plano UV.
            os(
                unsafe {
                    self.device
                        .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))
                },
                "CreateShaderResourceView",
            )?;
            srv.ok_or_else(|| RenderError::Other("SRV nula".into()))
        };
        Ok((make(DXGI_FORMAT_R8_UNORM)?, make(DXGI_FORMAT_R8G8_UNORM)?))
    }
}

fn swapchain_flags(tearing: bool) -> DXGI_SWAP_CHAIN_FLAG {
    let mut f = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;
    if tearing {
        f |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
    }
    DXGI_SWAP_CHAIN_FLAG(f)
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry_c = format!("{entry}\0");
    let target_c = format!("{target}\0");
    let mut code = None;
    let mut errors = None;
    // SAFETY: fuente y cadenas terminadas en NUL vivas durante la llamada.
    let r = unsafe {
        D3DCompile(
            SHADER_SRC.as_ptr() as *const _,
            SHADER_SRC.len(),
            PCSTR(c"shaders.hlsl".as_ptr() as *const u8),
            None,
            None,
            PCSTR(entry_c.as_ptr()),
            PCSTR(target_c.as_ptr()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = r {
        let msg = errors
            .map(|b| String::from_utf8_lossy(blob_bytes(&b)).into_owned())
            .unwrap_or_else(|| e.to_string());
        return Err(RenderError::Shader(format!("{entry}: {msg}")));
    }
    code.ok_or_else(|| RenderError::Shader(format!("{entry}: sin bytecode")))
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    // SAFETY: el blob es inmutable y vive tanto como la referencia devuelta.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}
