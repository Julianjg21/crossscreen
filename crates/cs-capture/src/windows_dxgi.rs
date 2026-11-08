//! Backend Windows: DXGI Desktop Duplication.
//!
//! Flujo por frame:
//! 1. `AcquireNextFrame` (bloquea hasta `timeout` si no hay cambios).
//! 2. Cursor: posición y, si cambió, forma (antes de liberar el frame).
//! 3. Metadatos: movimientos y regiones sucias.
//! 4. `CopyResource` de la superficie del escritorio a una textura propia
//!    del *pool* y `ReleaseFrame` enseguida, para que DWM pueda seguir.
//!
//! La imagen no sale de la GPU. El `ID3D11Device` se crea sobre el
//! adaptador que expone la salida (requisito de Desktop Duplication) y se
//! expone con [`DxgiCapture::device`] para que el codificador use el mismo.
//!
//! `DXGI_ERROR_ACCESS_LOST` (cambio de modo, de topología, escritorio
//! seguro...) se trata recreando la duplicación —y el dispositivo si la
//! salida cambió de adaptador— dentro de `next_frame`.
#![allow(unsafe_code)] // FFI DXGI/D3D11; cada bloque unsafe está acotado y comentado.

use crate::{
    ticks_to_us, utf16_z_to_string, CaptureConfig, CaptureError, CaptureSource, CaptureStats,
    CaptureTarget, CapturedFrame, CursorShape, CursorShapeKind, CursorState, FrameTiming,
    FrameUpdate, MoveRect, OutputInfo, PixelFormat, Rect,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{BOOL, E_ACCESSDENIED, E_INVALIDARG, HMODULE, LUID, POINT, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetPhysicalCursorPos, CURSORINFO, CURSOR_SHOWING,
};

fn os(call: &'static str, e: &windows::core::Error) -> CaptureError {
    CaptureError::Os {
        call,
        code: e.code().0 as u32,
    }
}

fn luid_i64(l: LUID) -> i64 {
    ((l.HighPart as i64) << 32) | l.LowPart as i64
}

fn rect_of(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn format_of(f: DXGI_FORMAT) -> PixelFormat {
    match f {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => PixelFormat::Bgra8,
        DXGI_FORMAT_R16G16B16A16_FLOAT => PixelFormat::Rgba16Float,
        other => PixelFormat::Other(other.0 as u32),
    }
}

/// Reloj QPC en microsegundos (mismo reloj que `LastPresentTime` de DXGI).
pub fn qpc_now_us() -> u64 {
    let (mut t, mut f) = (0i64, 0i64);
    // SAFETY: punteros a i64 válidos; QPC no falla en Windows XP+.
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    ticks_to_us(t, f)
}

fn qpc_freq() -> i64 {
    let mut f = 0i64;
    // SAFETY: puntero a i64 válido.
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    f
}

/// Consulta rápida (Win32) de la posición física del cursor (*hotspot*)
/// relativa al `desktop_rect` del monitor capturado.
///
/// Devuelve `Some((x, y, visible))` donde:
/// - `visible = true` si el cursor está visible en el sistema y dentro de
///   `desktop_rect` (con `x, y` en `[0, width) × [0, height)`);
/// - `visible = false` si el cursor está oculto o en otro monitor.
///
/// No toca D3D11 ni `IDXGIOutputDuplication`, por lo que puede invocarse a
/// 120–250 Hz desde cualquier hilo aunque el codificador esté ocupado o el
/// monitor virtual no dispare eventos DXGI de puntero.
pub fn poll_cursor_in_rect(desktop_rect: Rect) -> Option<(i32, i32, bool)> {
    let mut pt = POINT::default();
    // SAFETY: puntero a POINT local válido.
    unsafe { GetPhysicalCursorPos(&mut pt) }.ok()?;
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: estructura CURSORINFO con cbSize inicializado.
    let showing = unsafe { GetCursorInfo(&mut ci) }.is_ok() && (ci.flags.0 & CURSOR_SHOWING.0) != 0;
    let inside = pt.x >= desktop_rect.left
        && pt.x < desktop_rect.right
        && pt.y >= desktop_rect.top
        && pt.y < desktop_rect.bottom;
    if showing && inside {
        Some((pt.x - desktop_rect.left, pt.y - desktop_rect.top, true))
    } else {
        Some((0, 0, false))
    }
}

/// Errores tras los que tiene sentido reintentar la duplicación: la salida
/// está en transición (cambio de modo), en escritorio seguro (UAC, bloqueo)
/// o se perdió el dispositivo.
fn is_transient(code: HRESULT) -> bool {
    code == DXGI_ERROR_ACCESS_LOST
        || code == E_ACCESSDENIED
        || code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
        || code == DXGI_ERROR_SESSION_DISCONNECTED
        || code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
        || code == DXGI_ERROR_INVALID_CALL // salida desaparecida a mitad de crear
}

struct FoundOutput {
    info: OutputInfo,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
}

fn enumerate() -> Result<Vec<FoundOutput>, CaptureError> {
    // SAFETY: llamadas COM sin punteros crudos; los objetos devueltos se
    // gestionan por conteo de referencias.
    unsafe {
        // Fábrica nueva en cada enumeración: una vieja no ve cambios de
        // topología (IsCurrent() pasa a false).
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| os("CreateDXGIFactory1", &e))?;
        let mut found = Vec::new();
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let ad = adapter
                .GetDesc1()
                .map_err(|e| os("IDXGIAdapter1::GetDesc1", &e))?;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let od = output
                    .GetDesc()
                    .map_err(|e| os("IDXGIOutput::GetDesc", &e))?;
                found.push(FoundOutput {
                    info: OutputInfo {
                        gdi_name: utf16_z_to_string(&od.DeviceName),
                        adapter_name: utf16_z_to_string(&ad.Description),
                        adapter_luid: luid_i64(ad.AdapterLuid),
                        desktop_rect: rect_of(od.DesktopCoordinates),
                        attached_to_desktop: od.AttachedToDesktop.as_bool(),
                    },
                    adapter: adapter.clone(),
                    output,
                });
            }
        }
        Ok(found)
    }
}

/// Lista las salidas que DXGI ve, con su adaptador.
pub fn list_outputs() -> Result<Vec<OutputInfo>, CaptureError> {
    Ok(enumerate()?.into_iter().map(|f| f.info).collect())
}

fn find(target: &CaptureTarget) -> Result<FoundOutput, CaptureError> {
    enumerate()?
        .into_iter()
        .find(|f| f.info.attached_to_desktop && target.matches(&f.info))
        .ok_or_else(|| CaptureError::OutputNotFound(target.to_string()))
}

fn create_device(
    adapter: &IDXGIAdapter1,
) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device = None;
    let mut context = None;
    // SAFETY: punteros de salida a Option<Interface> válidos; con un
    // adaptador explícito el tipo de driver debe ser UNKNOWN.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| os("D3D11CreateDevice", &e))?;
    }
    let device = device.ok_or_else(|| CaptureError::Unsupported("D3D11 sin dispositivo".into()))?;
    let context = context.ok_or_else(|| CaptureError::Unsupported("D3D11 sin contexto".into()))?;
    // El codificador usará el mismo dispositivo desde otro hilo: activar la
    // protección multihilo del contexto inmediato.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: interfaz válida obtenida por QueryInterface.
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Ok((device, context))
}

/// Restaura el contexto DPI del hilo al salir del ámbito.
struct DpiGuard(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

impl DpiGuard {
    fn per_monitor_v2() -> Self {
        // SAFETY: cambia solo el contexto DPI del hilo actual; se restaura en Drop.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiGuard {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: restaura el contexto devuelto por la llamada anterior.
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

fn duplicate(
    output: &IDXGIOutput,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication, windows::core::Error> {
    // DuplicateOutput1 exige que el hilo sea DPI-aware por monitor (v2).
    let _dpi = DpiGuard::per_monitor_v2();
    let formats = [DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT];
    // SAFETY: llamadas COM con interfaces válidas.
    unsafe {
        match output.cast::<IDXGIOutput5>() {
            Ok(o5) => match o5.DuplicateOutput1(device, 0, &formats) {
                Ok(d) => Ok(d),
                // Algunos drivers no aceptan DuplicateOutput1: probar la v1 (solo BGRA8).
                Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => {
                    output.cast::<IDXGIOutput1>()?.DuplicateOutput(device)
                }
                Err(e) => Err(e),
            },
            Err(_) => output.cast::<IDXGIOutput1>()?.DuplicateOutput(device),
        }
    }
}

/// Textura D3D11 propia de la captura. Clonarla es barato (conteo de
/// referencias); mientras exista un clon, el *pool* no la reutiliza.
#[derive(Clone)]
pub struct D3D11Texture(Arc<TexSlot>);

struct TexSlot {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

// SAFETY: los recursos D3D11 son objetos libres de hilo (free-threaded); su
// conteo de referencias es atómico. Lo que no es seguro entre hilos es el
// contexto inmediato, que aquí no se comparte (y tiene la protección
// multihilo activada).
unsafe impl Send for TexSlot {}
// SAFETY: ver arriba; `&TexSlot` solo da acceso a la interfaz COM.
unsafe impl Sync for TexSlot {}

impl D3D11Texture {
    /// La textura nativa (`ID3D11Texture2D`, uso DEFAULT, BIND_SHADER_RESOURCE).
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.0.texture
    }
}

impl std::fmt::Debug for D3D11Texture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3D11Texture")
            .field("width", &self.0.width)
            .field("height", &self.0.height)
            .field("format", &self.0.format.0)
            .finish()
    }
}

/// Imagen leída a CPU (solo depuración).
#[derive(Debug, Clone)]
pub struct CpuImage {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

/// Descripción de la duplicación activa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicationInfo {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Rotación DXGI (1 = identidad).
    pub rotation: i32,
    /// La imagen del escritorio vive en memoria de sistema (p. ej. adaptador
    /// sin aceleración): la "captura en GPU" no es tal.
    pub desktop_in_system_memory: bool,
}

/// Captura de una salida con DXGI Desktop Duplication.
pub struct DxgiCapture {
    target: CaptureTarget,
    config: CaptureConfig,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter_luid: i64,
    output: OutputInfo,
    dupl: Option<IDXGIOutputDuplication>,
    dupl_info: Option<DuplicationInfo>,
    pool: Vec<D3D11Texture>,
    query: Option<ID3D11Query>,
    cursor: CursorState,
    shape_buf: Vec<u8>,
    sequence: u64,
    device_generation: u32,
    next_full: bool,
    stats: CaptureStats,
    lost_at: Option<Instant>,
    freq: i64,
}

// SAFETY: ID3D11Device es free-threaded. El contexto inmediato y la
// duplicación solo se usan desde `&mut self` (un hilo a la vez), y el
// contexto tiene la protección multihilo de D3D11 activada para cuando el
// codificador lo use desde otro hilo a través de `device()`.
unsafe impl Send for DxgiCapture {}

impl std::fmt::Debug for DxgiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapture")
            .field("target", &self.target)
            .field("output", &self.output)
            .field("duplication", &self.dupl_info)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl DxgiCapture {
    /// Abre la captura de `target`. Falla si la salida no existe o si
    /// Desktop Duplication no está disponible en su adaptador.
    pub fn new(target: CaptureTarget, config: CaptureConfig) -> Result<Self, CaptureError> {
        let found = find(&target)?;
        let (device, context) = create_device(&found.adapter)?;
        let mut me = Self {
            target,
            config,
            device,
            context,
            adapter_luid: found.info.adapter_luid,
            output: found.info.clone(),
            dupl: None,
            dupl_info: None,
            pool: Vec::new(),
            query: None,
            cursor: CursorState::default(),
            shape_buf: Vec::new(),
            sequence: 0,
            device_generation: 0,
            next_full: true,
            stats: CaptureStats::default(),
            lost_at: None,
            freq: qpc_freq(),
        };
        me.open_duplication(&found).map_err(|e| {
            if e.code() == DXGI_ERROR_UNSUPPORTED {
                CaptureError::Unsupported(format!(
                    "Desktop Duplication no soportado en {} ({}); ¿adaptador equivocado en un equipo híbrido?",
                    me.output.gdi_name, me.output.adapter_name
                ))
            } else if e.code() == E_INVALIDARG {
                CaptureError::AlreadyCapturing(me.output.gdi_name.clone())
            } else {
                os("DuplicateOutput", &e)
            }
        })?;
        Ok(me)
    }

    /// Dispositivo D3D11 sobre el que viven las texturas capturadas. El
    /// codificador debe usar este mismo (mismo adaptador, sin copias).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Contexto inmediato del dispositivo (protección multihilo activada).
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Descripción de la duplicación activa (None mientras se recupera).
    pub fn duplication_info(&self) -> Option<DuplicationInfo> {
        self.dupl_info
    }

    fn open_duplication(&mut self, found: &FoundOutput) -> Result<(), windows::core::Error> {
        let dupl = duplicate(&found.output, &self.device)?;
        // SAFETY: interfaz válida recién creada.
        let d = unsafe { dupl.GetDesc() };
        self.dupl_info = Some(DuplicationInfo {
            width: d.ModeDesc.Width,
            height: d.ModeDesc.Height,
            format: format_of(d.ModeDesc.Format),
            rotation: d.Rotation.0,
            desktop_in_system_memory: d.DesktopImageInSystemMemory.as_bool(),
        });
        tracing::debug!(output = %found.info.gdi_name, info = ?self.dupl_info, "duplicación creada");
        self.output = found.info.clone();
        self.dupl = Some(dupl);
        self.next_full = true;
        Ok(())
    }

    /// Intenta recrear la duplicación tras una pérdida.
    fn try_reinit(&mut self) -> Result<(), CaptureError> {
        let found = find(&self.target)?;
        if found.info.adapter_luid != self.adapter_luid {
            // La salida cambió de GPU: hace falta un dispositivo nuevo.
            let (device, context) = create_device(&found.adapter)?;
            self.device = device;
            self.context = context;
            self.adapter_luid = found.info.adapter_luid;
            self.pool.clear();
            self.query = None;
            self.device_generation += 1;
            self.stats.device_recreations += 1;
        }
        match self.open_duplication(&found) {
            Ok(()) => {
                self.stats.reinits += 1;
                Ok(())
            }
            Err(e)
                if e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET =>
            {
                // Forzar dispositivo nuevo en el próximo intento.
                self.adapter_luid = 0;
                Err(os("DuplicateOutput", &e))
            }
            Err(e) => Err(os("DuplicateOutput", &e)),
        }
    }

    fn mark_lost(&mut self) {
        self.dupl = None;
        self.dupl_info = None;
        self.stats.access_lost += 1;
        if self.lost_at.is_none() {
            self.lost_at = Some(Instant::now());
        }
    }

    fn update_cursor(&mut self, dupl: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let pos = Some((p.Position.x, p.Position.y));
            let vis = p.Visible.as_bool();
            if pos != self.cursor.position || vis != self.cursor.visible {
                self.cursor.position = pos;
                self.cursor.visible = vis;
                self.cursor.position_generation += 1;
            }
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        self.shape_buf
            .resize(info.PointerShapeBufferSize as usize, 0);
        let mut required = 0u32;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: buffer de `PointerShapeBufferSize` bytes; el frame sigue adquirido.
        let r = unsafe {
            dupl.GetFramePointerShape(
                self.shape_buf.len() as u32,
                self.shape_buf.as_mut_ptr().cast(),
                &mut required,
                &mut si,
            )
        };
        if let Err(e) = r {
            tracing::warn!("GetFramePointerShape: {e}");
            return;
        }
        let kind = match DXGI_OUTDUPL_POINTER_SHAPE_TYPE(si.Type as i32) {
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => CursorShapeKind::Monochrome,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR => CursorShapeKind::Color,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => CursorShapeKind::MaskedColor,
            _ => return,
        };
        self.cursor.shape = Some(CursorShape {
            kind,
            width: si.Width,
            height: si.Height,
            pitch: si.Pitch,
            hotspot_x: si.HotSpot.x,
            hotspot_y: si.HotSpot.y,
            data: self.shape_buf[..(required as usize).min(self.shape_buf.len())].to_vec(),
        });
        self.cursor.shape_generation += 1;
    }

    fn read_metadata(
        &mut self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> FrameUpdate {
        let full = FrameUpdate {
            full: true,
            ..Default::default()
        };
        if std::mem::take(&mut self.next_full) || !self.config.collect_metadata {
            return full;
        }
        let total = info.TotalMetadataBufferSize as usize;
        if total == 0 {
            return full;
        }
        let msz = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let rsz = std::mem::size_of::<RECT>();
        let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); total / msz + 1];
        let mut required = 0u32;
        // SAFETY: buffer de `moves.len() * msz` bytes; frame adquirido.
        let r = unsafe {
            dupl.GetFrameMoveRects(
                (moves.len() * msz) as u32,
                moves.as_mut_ptr(),
                &mut required,
            )
        };
        if r.is_err() {
            return full;
        }
        moves.truncate(required as usize / msz);
        let mut dirty = vec![RECT::default(); total / rsz + 1];
        // SAFETY: ídem con RECT.
        let r = unsafe {
            dupl.GetFrameDirtyRects(
                (dirty.len() * rsz) as u32,
                dirty.as_mut_ptr(),
                &mut required,
            )
        };
        if r.is_err() {
            return full;
        }
        dirty.truncate(required as usize / rsz);
        FrameUpdate {
            full: false,
            moves: moves
                .iter()
                .map(|m| MoveRect {
                    source_x: m.SourcePoint.x,
                    source_y: m.SourcePoint.y,
                    destination: rect_of(m.DestinationRect),
                })
                .collect(),
            dirty: dirty.iter().copied().map(rect_of).collect(),
        }
    }

    /// Devuelve una textura libre del *pool* con la descripción dada.
    fn slot_for(&mut self, src: &D3D11_TEXTURE2D_DESC) -> Result<D3D11Texture, CaptureError> {
        self.pool.retain(|t| {
            t.0.width == src.Width && t.0.height == src.Height && t.0.format == src.Format
        });
        if let Some(t) = self.pool.iter().find(|t| Arc::strong_count(&t.0) == 1) {
            return Ok(t.clone());
        }
        if self.pool.len() >= self.config.max_pool_textures.max(1) {
            return Err(CaptureError::Unsupported(format!(
                "las {} texturas del pool siguen en uso: el consumidor no suelta los frames",
                self.pool.len()
            )));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.Width,
            Height: src.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: descripción válida, puntero de salida válido.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| os("CreateTexture2D", &e))?;
        let tex = tex.ok_or_else(|| CaptureError::Unsupported("textura nula".into()))?;
        let t = D3D11Texture(Arc::new(TexSlot {
            texture: tex,
            width: src.Width,
            height: src.Height,
            format: src.Format,
        }));
        self.pool.push(t.clone());
        Ok(t)
    }

    fn wait_gpu(&mut self) -> Result<(), CaptureError> {
        if self.query.is_none() {
            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut q = None;
            // SAFETY: descripción y puntero de salida válidos.
            unsafe { self.device.CreateQuery(&desc, Some(&mut q)) }
                .map_err(|e| os("CreateQuery", &e))?;
            self.query = q;
        }
        let Some(q) = self.query.as_ref() else {
            return Ok(());
        };
        // SAFETY: consulta válida del mismo dispositivo; `done` es un BOOL
        // del tamaño que espera D3D11_QUERY_EVENT.
        unsafe {
            self.context.End(q);
            let start = Instant::now();
            loop {
                let mut done = BOOL(0);
                let r = self.context.GetData(
                    q,
                    Some((&mut done as *mut BOOL).cast()),
                    std::mem::size_of::<BOOL>() as u32,
                    0, // sin DONOTFLUSH: la primera llamada vacía la cola
                );
                if let Err(e) = r {
                    return Err(os("GetData", &e));
                }
                if done.as_bool() || start.elapsed() > Duration::from_millis(500) {
                    return Ok(());
                }
                std::hint::spin_loop();
            }
        }
    }

    /// Lee una textura capturada a CPU (staging + Map). **Solo depuración.**
    pub fn read_to_cpu(&self, tex: &D3D11Texture) -> Result<CpuImage, CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: tex.0.width,
            Height: tex.0.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: tex.0.format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let fmt = format_of(tex.0.format);
        let bpp = fmt
            .bytes_per_pixel()
            .ok_or_else(|| CaptureError::Unsupported(format!("formato {fmt:?}")))?
            as usize;
        // SAFETY: recursos del mismo dispositivo; el Map se deshace antes de
        // salir y solo se leen `height` filas de `width*bpp` bytes dentro del
        // `RowPitch` informado.
        unsafe {
            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| os("CreateTexture2D(staging)", &e))?;
            let staging =
                staging.ok_or_else(|| CaptureError::Unsupported("staging nula".into()))?;
            self.context.CopyResource(&staging, &tex.0.texture);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .map_err(|e| os("Map", &e))?;
            let row = tex.0.width as usize * bpp;
            let pitch = map.RowPitch as usize;
            let mut data = vec![0u8; row * tex.0.height as usize];
            for y in 0..tex.0.height as usize {
                let src = std::slice::from_raw_parts((map.pData as *const u8).add(y * pitch), row);
                data[y * row..(y + 1) * row].copy_from_slice(src);
            }
            self.context.Unmap(&staging, 0);
            Ok(CpuImage {
                width: tex.0.width,
                height: tex.0.height,
                stride: row,
                format: fmt,
                data,
            })
        }
    }

    fn now_us(&self) -> u64 {
        let mut t = 0i64;
        // SAFETY: puntero a i64 válido.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        ticks_to_us(t, self.freq)
    }
}

impl CaptureSource for DxgiCapture {
    type Texture = D3D11Texture;

    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame<D3D11Texture>>, CaptureError> {
        self.stats.calls += 1;
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if self.dupl.is_none() {
                match self.try_reinit() {
                    Ok(()) => {
                        self.stats.last_reinit_latency = self.lost_at.map(|t| t.elapsed());
                    }
                    Err(e) => {
                        self.stats.failed_reinits += 1;
                        self.stats.last_reinit_error = Some(e.to_string());
                        let transient = match &e {
                            CaptureError::OutputNotFound(_) => true,
                            CaptureError::Os { code, .. } => is_transient(HRESULT(*code as i32)),
                            _ => false,
                        };
                        let lost_for = self.lost_at.map(|t| t.elapsed()).unwrap_or_default();
                        if !transient {
                            return Err(e);
                        }
                        if lost_for > self.config.recovery_grace {
                            return Err(CaptureError::RecoveryTimeout(lost_for));
                        }
                        if now >= deadline {
                            return Ok(None);
                        }
                        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
                        continue;
                    }
                }
            }
            let Some(dupl) = self.dupl.clone() else {
                continue;
            };
            let wait_ms = deadline.saturating_duration_since(now).as_millis() as u32;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: punteros de salida válidos; no hay otro frame adquirido
            // (siempre se libera antes de volver).
            let r = unsafe { dupl.AcquireNextFrame(wait_ms, &mut info, &mut resource) };
            if let Err(e) = r {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    self.stats.timeouts += 1;
                    return Ok(None);
                }
                if is_transient(e.code()) {
                    tracing::info!("AcquireNextFrame: {e}; recreando la duplicación");
                    self.mark_lost();
                    continue;
                }
                return Err(os("AcquireNextFrame", &e));
            }
            let acquired_us = self.now_us();
            self.update_cursor(&dupl, &info);

            if info.LastPresentTime == 0 || resource.is_none() {
                // Solo cambió el cursor: devolver enseguida para publicar
                // posición/forma sin esperar a que venza `timeout`.
                self.stats.pointer_only_updates += 1;
                drop(resource);
                // SAFETY: hay un frame adquirido que liberar.
                if let Err(e) = unsafe { dupl.ReleaseFrame() } {
                    if is_transient(e.code()) {
                        self.mark_lost();
                    }
                }
                return Ok(None);
            }

            let update = self.read_metadata(&dupl, &info);
            let copy = (|| -> Result<D3D11Texture, CaptureError> {
                let src: ID3D11Texture2D = resource
                    .as_ref()
                    .ok_or_else(|| CaptureError::Unsupported("frame sin recurso".into()))?
                    .cast()
                    .map_err(|e| os("QueryInterface(ID3D11Texture2D)", &e))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: textura válida, puntero de salida válido.
                unsafe { src.GetDesc(&mut desc) };
                let slot = self.slot_for(&desc)?;
                // SAFETY: mismas dimensiones y formato, mismo dispositivo.
                unsafe { self.context.CopyResource(&slot.0.texture, &src) };
                Ok(slot)
            })();
            drop(resource);
            // SAFETY: hay un frame adquirido; la copia ya está encolada y D3D11
            // mantiene viva la superficie origen hasta ejecutarla.
            let released = unsafe { dupl.ReleaseFrame() };
            let texture = copy?;
            if let Err(e) = released {
                if is_transient(e.code()) {
                    self.mark_lost();
                } else {
                    return Err(os("ReleaseFrame", &e));
                }
            }
            if self.config.wait_for_gpu_copy {
                self.wait_gpu()?;
            }
            let ready_us = self.now_us();
            if let Some(t) = self.lost_at.take() {
                self.stats.last_recovery = Some(t.elapsed());
            }
            self.sequence += 1;
            self.stats.frames += 1;
            return Ok(Some(CapturedFrame {
                width: texture.0.width,
                height: texture.0.height,
                format: format_of(texture.0.format),
                texture,
                sequence: self.sequence,
                accumulated_frames: info.AccumulatedFrames,
                protected_content_masked: info.ProtectedContentMaskedOut.as_bool(),
                timing: FrameTiming {
                    present_us: ticks_to_us(info.LastPresentTime, self.freq),
                    acquired_us,
                    ready_us,
                },
                update,
                device_generation: self.device_generation,
            }));
        }
    }

    fn cursor(&self) -> &CursorState {
        &self.cursor
    }

    fn output(&self) -> &OutputInfo {
        &self.output
    }

    fn stats(&self) -> &CaptureStats {
        &self.stats
    }
}
