//! Conversión BGRA → NV12 en la GPU con el Video Processor de D3D11.
//!
//! El Video Processor es la vía estándar (y la que usa la propia GPU para
//! vídeo): en Intel lo ejecuta el bloque de vídeo (VEBOX/SFC) o el EU, sin
//! pasar por la CPU. La salida es un *pool* de texturas NV12 que se entregan
//! en rotación al codificador.
#![allow(unsafe_code)] // FFI D3D11; cada bloque unsafe está acotado y comentado.

use super::hr_err;
use crate::{EncodeError, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, FALSE, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Número máximo de vistas de entrada cacheadas (una por textura de origen).
const INPUT_VIEW_CACHE: usize = 16;

/// Conversor BGRA → NV12 con *pool* propio de texturas NV12.
pub struct Nv12Converter {
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    video_device: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Vistas de entrada cacheadas. Guardar la textura mantiene viva la
    /// referencia COM, así que el puntero no puede reutilizarse mientras esté aquí.
    input_views: Vec<(ID3D11Texture2D, u32, ID3D11VideoProcessorInputView)>,
    in_size: (u32, u32),
    out_size: (u32, u32),
    /// `true` si se usó `ID3D11VideoContext1` para fijar espacios de color DXGI.
    pub color_space_api1: bool,
    /// Banderas de enlace con las que se crearon las texturas NV12.
    pub nv12_bind_flags: u32,
}

impl std::fmt::Debug for Nv12Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nv12Converter")
            .field("in_size", &self.in_size)
            .field("out_size", &self.out_size)
            .field("pool", &self.pool.len())
            .finish()
    }
}

impl Nv12Converter {
    /// Crea el conversor sobre `device` (que debe tener soporte de vídeo,
    /// `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`). Si los tamaños difieren, escala.
    pub fn new(
        device: &ID3D11Device,
        in_size: (u32, u32),
        out_size: (u32, u32),
        fps: (u32, u32),
        pool_size: usize,
    ) -> Result<Self> {
        if out_size.0 % 2 != 0 || out_size.1 % 2 != 0 {
            return Err(EncodeError::BadInput(format!(
                "NV12 exige dimensiones pares: {}x{}",
                out_size.0, out_size.1
            )));
        }
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| hr_err("ID3D11Device→ID3D11VideoDevice", e))?;
        // SAFETY: llamada COM sobre un dispositivo válido.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| hr_err("GetImmediateContext", e))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| hr_err("ID3D11DeviceContext→ID3D11VideoContext", e))?;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps.0,
                Denominator: fps.1,
            },
            InputWidth: in_size.0,
            InputHeight: in_size.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps.0,
                Denominator: fps.1,
            },
            OutputWidth: out_size.0,
            OutputHeight: out_size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: `desc` vive durante la llamada.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .map_err(|e| hr_err("CreateVideoProcessorEnumerator", e))?;

        // SAFETY: consultas de formato sobre un enumerador válido.
        let bgra = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
            .map_err(|e| hr_err("CheckVideoProcessorFormat(BGRA)", e))?;
        // SAFETY: ídem.
        let nv12 = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
            .map_err(|e| hr_err("CheckVideoProcessorFormat(NV12)", e))?;
        if bgra & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
            return Err(EncodeError::Unsupported(
                "el Video Processor no acepta BGRA como entrada".into(),
            ));
        }
        if nv12 & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            return Err(EncodeError::Unsupported(
                "el Video Processor no produce NV12".into(),
            ));
        }

        // SAFETY: enumerador válido; índice 0 de conversión de frecuencia.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| hr_err("CreateVideoProcessor", e))?;

        // Espacios de color: RGB completo sRGB → YCbCr BT.709 de rango limitado,
        // que es lo que se declara al codificador (VUI).
        let mut color_space_api1 = false;
        if let Ok(vc1) = video_context.cast::<ID3D11VideoContext1>() {
            // SAFETY: procesador válido; flujo 0 existe.
            unsafe {
                vc1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                vc1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }
            color_space_api1 = true;
        } else {
            // Campo de bits D3D11_VIDEO_PROCESSOR_COLOR_SPACE:
            // bit0 Usage, bit1 RGB_Range (0 = completo), bit2 YCbCr_Matrix
            // (1 = BT.709), bits4-5 Nominal_Range (1 = 16-235).
            let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            // SAFETY: punteros a estructuras locales válidas.
            unsafe {
                video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input);
                video_context.VideoProcessorSetOutputColorSpace(&processor, &output);
            }
        }
        // SAFETY: procesador válido.
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // Sin "mejoras" automáticas del driver (denoise, realce…): coste y latencia.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, FALSE);
        }

        let mut conv = Self {
            context,
            video_context,
            video_device,
            enumerator,
            processor,
            pool: Vec::with_capacity(pool_size),
            next: 0,
            input_views: Vec::new(),
            in_size,
            out_size,
            color_space_api1,
            nv12_bind_flags: 0,
        };
        conv.create_pool(device, pool_size.max(2))?;
        Ok(conv)
    }

    fn create_pool(&mut self, device: &ID3D11Device, n: usize) -> Result<()> {
        // Primero con BIND_VIDEO_ENCODER (ayuda a algunos drivers a elegir un
        // *layout* apto para el codificador); si falla, solo RENDER_TARGET.
        let candidates = [
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
            D3D11_BIND_RENDER_TARGET.0 as u32,
        ];
        let mut last_err = None;
        for bind in candidates {
            self.pool.clear();
            match self.try_create_pool(device, n, bind) {
                Ok(()) => {
                    self.nv12_bind_flags = bind;
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(EncodeError::Unsupported("pool NV12".into())))
    }

    fn try_create_pool(&mut self, device: &ID3D11Device, n: usize, bind: u32) -> Result<()> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.out_size.0,
            Height: self.out_size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..n {
            let mut tex = None;
            // SAFETY: `desc` válido; puntero de salida a un Option local.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .map_err(|e| hr_err("CreateTexture2D(NV12)", e))?;
            let tex = tex.ok_or(EncodeError::Unsupported(
                "CreateTexture2D(NV12) nulo".into(),
            ))?;
            let vdesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: textura y enumerador válidos; `vdesc` vive durante la llamada.
            unsafe {
                self.video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &self.enumerator,
                    &vdesc,
                    Some(&mut view),
                )
            }
            .map_err(|e| hr_err("CreateVideoProcessorOutputView", e))?;
            let view = view.ok_or(EncodeError::Unsupported("vista de salida nula".into()))?;
            self.pool.push((tex, view));
        }
        Ok(())
    }

    fn input_view(
        &mut self,
        src: &ID3D11Texture2D,
        subresource: u32,
    ) -> Result<ID3D11VideoProcessorInputView> {
        if let Some((_, _, v)) = self
            .input_views
            .iter()
            .find(|(t, s, _)| t.as_raw() == src.as_raw() && *s == subresource)
        {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: subresource,
                },
            },
        };
        let mut view = None;
        // SAFETY: textura y enumerador válidos; `desc` vive durante la llamada.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                src,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|e| hr_err("CreateVideoProcessorInputView", e))?;
        let view = view.ok_or(EncodeError::Unsupported("vista de entrada nula".into()))?;
        if self.input_views.len() >= INPUT_VIEW_CACHE {
            self.input_views.remove(0);
        }
        self.input_views
            .push((src.clone(), subresource, view.clone()));
        Ok(view)
    }

    /// Convierte `src` (BGRA del mismo dispositivo) en la siguiente textura NV12
    /// del *pool* y la devuelve. Solo encola trabajo en la GPU; no espera.
    pub fn convert(&mut self, src: &ID3D11Texture2D, subresource: u32) -> Result<ID3D11Texture2D> {
        let input = self.input_view(src, subresource)?;
        let (tex, out_view) = self.pool[self.next].clone();
        self.next = (self.next + 1) % self.pool.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        // SAFETY: procesador, vista de salida y vista de entrada válidos; el
        // array de flujos vive durante la llamada.
        let r = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &out_view, 0, &streams)
        };
        // SAFETY: liberamos la referencia que pusimos en ManuallyDrop.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.map_err(|e| hr_err("VideoProcessorBlt", e))?;
        Ok(tex)
    }

    /// Envía a la GPU el trabajo encolado (no espera a que termine).
    pub fn flush(&self) {
        // SAFETY: contexto inmediato válido.
        unsafe { self.context.Flush() };
    }

    /// Tamaño del *pool* NV12.
    pub fn pool_len(&self) -> usize {
        self.pool.len()
    }

    /// Contexto inmediato (útil para medir con consultas de tiempo).
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
}

/// Activa la protección multihilo del dispositivo: el MFT usa el mismo
/// dispositivo desde sus propios hilos. Devuelve el estado anterior.
pub fn enable_multithread_protection(device: &ID3D11Device) -> Result<bool> {
    let mt: ID3D11Multithread = device
        .cast()
        .map_err(|e| hr_err("ID3D11Device→ID3D11Multithread", e))?;
    // SAFETY: interfaz válida.
    let prev: BOOL = unsafe { mt.SetMultithreadProtected(TRUE) };
    Ok(prev.as_bool())
}
