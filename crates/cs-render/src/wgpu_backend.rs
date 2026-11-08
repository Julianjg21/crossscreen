//! Presentación con `wgpu` (Linux y otras plataformas que no son Windows).
//!
//! - Entrada: planos I420 en memoria de CPU (de OpenH264); se suben a tres
//!   texturas `R8Unorm` (Y a resolución completa, U y V a la mitad).
//! - Un *shader* convierte YUV (BT.709, rango limitado) a RGB y dibuja el
//!   vídeo en el rectángulo de [`fit_rect`](crate::fit_rect).
//! - El cursor llega fuera de banda y se dibuja encima con su imagen RGBA
//!   (premultiplicada) y mezcla alfa. Hasta recibir la primera forma no se
//!   dibuja cursor.
//!
//! No hay *zero-copy* en este backend (la decodificación es por CPU); el
//! siguiente paso previsto es VAAPI → DMA-BUF → Vulkan.

use crate::{cursor_rect, fit_rect, premultiply_rgba, RenderError, Result};
use std::time::{Duration, Instant};

/// Frame a presentar: planos I420 en CPU.
#[derive(Debug, Clone, Copy)]
pub struct FrameRef<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    /// Bytes por fila de `y`, `u` y `v`.
    pub strides: (usize, usize, usize),
    pub width: u32,
    pub height: u32,
}

/// Opciones del renderizador.
#[derive(Debug, Clone, Copy)]
pub struct RendererOptions {
    /// Sincronizar con el refresco (`Fifo`); si no, `Mailbox`/`Immediate`.
    pub vsync: bool,
}

impl Default for RendererOptions {
    fn default() -> Self {
        RendererOptions { vsync: true }
    }
}

/// Medidas de una presentación.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresentStats {
    /// Tiempo de CPU de la llamada de presentación.
    pub cpu_submit: Duration,
    /// No disponible con wgpu (siempre `None`).
    pub present_to_vblank: Option<Duration>,
}

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Triángulo que cubre el *viewport*; el rectángulo lo fija set_viewport.
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.pos = vec4<f32>(p[i], 0.0, 1.0);
    out.uv = vec2<f32>((p[i].x + 1.0) * 0.5, 1.0 - (p[i].y + 1.0) * 0.5);
    return out;
}

@group(0) @binding(0) var t_y: texture_2d<f32>;
@group(0) @binding(1) var t_u: texture_2d<f32>;
@group(0) @binding(2) var t_v: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;

@fragment
fn fs_video(in: VsOut) -> @location(0) vec4<f32> {
    // BT.709 rango limitado (16-235 / 16-240).
    let y = (textureSample(t_y, smp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(t_u, smp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (textureSample(t_v, smp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@group(0) @binding(0) var t_cursor: texture_2d<f32>;
@group(0) @binding(1) var smp_cursor: sampler;

@fragment
fn fs_cursor(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(t_cursor, smp_cursor, in.uv);
}
"#;

struct VideoTextures {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

struct CursorTexture {
    size: (u32, u32),
    hotspot: (u32, u32),
    bind_group: wgpu::BindGroup,
}

/// Renderizador wgpu de una ventana.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    video_pipeline: wgpu::RenderPipeline,
    cursor_pipeline: wgpu::RenderPipeline,
    video_layout: wgpu::BindGroupLayout,
    cursor_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    video: Option<VideoTextures>,
    cursor: Option<CursorTexture>,
    cursor_pos: (f32, f32),
    cursor_visible: bool,
    pending: Option<wgpu::SurfaceTexture>,
    adapter_info: String,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("adapter", &self.adapter_info)
            .field("size", &(self.config.width, self.config.height))
            .finish_non_exhaustive()
    }
}

fn other(msg: impl std::fmt::Display) -> RenderError {
    RenderError::Other(msg.to_string())
}

impl Renderer {
    /// Crea el renderizador sobre `target` (p. ej. un `Arc<winit::window::Window>`).
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        options: RendererOptions,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(target)
            .map_err(|e| other(format!("creando la superficie: {e}")))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| other(format!("no hay adaptador gráfico compatible: {e}")))?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
        tracing::info!("renderer wgpu: {adapter_info}");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("crossscreen-viewer"),
            ..Default::default()
        }))
        .map_err(|e| other(format!("creando el dispositivo: {e}")))?;

        let caps = surface.get_capabilities(&adapter);
        // Formato sin conversión sRGB: el vídeo ya viene codificado en gamma.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| other("la superficie no ofrece ningún formato"))?;
        let present_mode = if options.vsync {
            wgpu::PresentMode::Fifo
        } else if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cs-render"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let smp_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let video_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video"),
            entries: &[tex_entry(0), tex_entry(1), tex_entry(2), smp_entry(3)],
        });
        let cursor_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cursor"),
            entries: &[tex_entry(0), smp_entry(1)],
        });
        let pipeline = |layout: &wgpu::BindGroupLayout, fs: &str, blend: wgpu::BlendState| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(fs),
                bind_group_layouts: &[layout],
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let video_pipeline = pipeline(&video_layout, "fs_video", wgpu::BlendState::REPLACE);
        let cursor_pipeline = pipeline(
            &cursor_layout,
            "fs_cursor",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lineal"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Renderer {
            surface,
            device,
            queue,
            config,
            video_pipeline,
            cursor_pipeline,
            video_layout,
            cursor_layout,
            sampler,
            video: None,
            cursor: None,
            cursor_pos: (0.0, 0.0),
            cursor_visible: false,
            pending: None,
            adapter_info,
        })
    }

    /// Adaptador usado (para el registro).
    pub fn adapter_info(&self) -> &str {
        &self.adapter_info
    }

    /// Este backend sube los frames desde la CPU.
    pub fn zero_copy(&self) -> Option<bool> {
        Some(false)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.pending = None;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        Ok(())
    }

    pub fn set_cursor(&mut self, pos: (f32, f32), visible: bool) {
        self.cursor_pos = pos;
        self.cursor_visible = visible;
    }

    pub fn set_cursor_shape(
        &mut self,
        width: u32,
        height: u32,
        hotspot: (u32, u32),
        rgba: &[u8],
    ) -> Result<()> {
        if width == 0 || height == 0 || rgba.len() != (width * height * 4) as usize {
            return Err(other("forma de cursor con tamaño inconsistente"));
        }
        let tex = self.texture("cursor", width, height, wgpu::TextureFormat::Rgba8Unorm);
        let data = premultiply_rgba(rgba);
        self.upload(&tex, &data, (width * 4) as usize, width, height);
        let view = tex.create_view(&Default::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cursor"),
            layout: &self.cursor_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.cursor = Some(CursorTexture {
            size: (width, height),
            hotspot,
            bind_group,
        });
        Ok(())
    }

    pub fn has_cursor_shape(&self) -> bool {
        self.cursor.is_some()
    }

    /// Sin objeto *waitable*: el ritmo lo marca `get_current_texture` (Fifo).
    pub fn wait_for_frame(&self, _timeout: Duration) -> bool {
        true
    }

    fn texture(&self, label: &str, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn upload(&self, tex: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    fn ensure_video_textures(&mut self, w: u32, h: u32) {
        if self
            .video
            .as_ref()
            .is_some_and(|v| v.width == w && v.height == h)
        {
            return;
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = self.texture("y", w, h, wgpu::TextureFormat::R8Unorm);
        let u = self.texture("u", cw, ch, wgpu::TextureFormat::R8Unorm);
        let v = self.texture("v", cw, ch, wgpu::TextureFormat::R8Unorm);
        let (vy, vu, vv) = (
            y.create_view(&Default::default()),
            u.create_view(&Default::default()),
            v.create_view(&Default::default()),
        );
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &self.video_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&vy),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&vu),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&vv),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.video = Some(VideoTextures {
            width: w,
            height: h,
            y,
            u,
            v,
            bind_group,
        });
    }

    /// Sube el frame (si hay) y dibuja vídeo + cursor en el siguiente
    /// *back buffer*. La presentación ocurre en [`present`](Self::present).
    pub fn render(&mut self, frame: Option<FrameRef<'_>>) -> Result<()> {
        if let Some(f) = frame {
            let (w, h) = (f.width, f.height);
            if w == 0 || h == 0 {
                return Err(other("frame de tamaño 0"));
            }
            self.ensure_video_textures(w, h);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            if let Some(v) = self.video.as_ref() {
                self.upload(&v.y, f.y, f.strides.0, w, h);
                self.upload(&v.u, f.u, f.strides.1, cw, ch);
                self.upload(&v.v, f.v, f.strides.2, cw, ch);
            }
        }
        let surface_tex = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.config);
                self.surface
                    .get_current_texture()
                    .map_err(|e| other(format!("superficie: {e}")))?
            }
            Err(e) => return Err(other(format!("superficie: {e}"))),
        };
        let view = surface_tex.texture.create_view(&Default::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let (ww, wh) = (self.config.width, self.config.height);
            if let Some(v) = self.video.as_ref() {
                let (x, y, w, h) = fit_rect(v.width, v.height, ww, wh);
                if w >= 1.0 && h >= 1.0 {
                    pass.set_viewport(x, y, w, h, 0.0, 1.0);
                    pass.set_pipeline(&self.video_pipeline);
                    pass.set_bind_group(0, &v.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                if let (Some(c), true) = (self.cursor.as_ref(), self.cursor_visible) {
                    let r = cursor_rect(
                        self.cursor_pos,
                        (c.hotspot.0 as f32, c.hotspot.1 as f32),
                        (c.size.0 as f32, c.size.1 as f32),
                        (x, y, w, h),
                        v.width,
                    );
                    // El viewport debe quedar dentro del destino.
                    let (cx, cy) = (r.0.max(0.0), r.1.max(0.0));
                    let (cw, chh) = (
                        (r.2 - (cx - r.0)).min(ww as f32 - cx),
                        (r.3 - (cy - r.1)).min(wh as f32 - cy),
                    );
                    if cw >= 1.0 && chh >= 1.0 && cx < ww as f32 && cy < wh as f32 {
                        pass.set_viewport(cx, cy, cw, chh, 0.0, 1.0);
                        pass.set_pipeline(&self.cursor_pipeline);
                        pass.set_bind_group(0, &c.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }
            }
        }
        self.queue.submit(Some(enc.finish()));
        self.pending = Some(surface_tex);
        Ok(())
    }

    /// Presenta lo dibujado en [`render`](Self::render).
    pub fn present(&mut self) -> Result<PresentStats> {
        let start = Instant::now();
        if let Some(t) = self.pending.take() {
            t.present();
        }
        Ok(PresentStats {
            cpu_submit: start.elapsed(),
            present_to_vblank: None,
        })
    }

    /// No disponible con wgpu.
    pub fn time_to_next_vblank(&self, _period: Duration) -> Option<Duration> {
        None
    }

    /// Captura del *back buffer*: aún no implementada en este backend.
    pub fn capture_rgba(&self) -> Result<(u32, u32, Vec<u8>)> {
        Err(other(
            "la captura de pantalla del visor aún no está implementada en Linux",
        ))
    }
}
