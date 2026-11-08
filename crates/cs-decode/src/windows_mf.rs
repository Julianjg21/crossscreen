//! Backend Windows: MFT H.264 de Media Foundation con DXVA sobre D3D11.
//!
//! Flujo:
//! 1. `create_d3d11_device` crea un dispositivo D3D11 hardware con
//!    `VIDEO_SUPPORT` y protección multihilo. El **mismo** dispositivo lo usa
//!    `cs-render`, así la textura decodificada se muestrea sin copias.
//! 2. `VideoDecoder::new` instancia `CLSID_CMSH264DecoderMFT`, le pasa un
//!    `IMFDXGIDeviceManager` (`MFT_MESSAGE_SET_D3D_MANAGER`), activa baja
//!    latencia y pide superficies con `D3D11_BIND_SHADER_RESOURCE` además de
//!    `D3D11_BIND_DECODER` (`MF_SA_D3D11_BINDFLAGS`).
//! 3. `decode` entrega la AU (con SPS/PPS en línea si los trae), recoge la
//!    salida y gestiona `MF_E_TRANSFORM_STREAM_CHANGE` (primer frame y
//!    cambios de resolución) y `MF_E_TRANSFORM_NEED_MORE_INPUT`.
//!
//! El frame de salida retiene el `IMFSample`: mientras viva, el MFT no
//! reutiliza esa superficie. Hay que soltarlo en cuanto se haya presentado.
#![allow(unsafe_code)] // FFI COM/Media Foundation/D3D11; cada bloque unsafe está acotado y comentado.

use crate::{DecodeError, Result};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, HRESULT, VARIANT};
use windows::Win32::Foundation::{BOOL, HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// `CLSID_CMSH264DecoderMFT` ({62CE7E72-4C71-4D20-B15D-452831A87D9D}); no
/// está en `windows` 0.58.
const CLSID_CMSH264_DECODER_MFT: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);

pub(crate) trait HrContext<T> {
    fn ctx(self, call: &'static str) -> Result<T>;
}

impl<T> HrContext<T> for windows::core::Result<T> {
    fn ctx(self, call: &'static str) -> Result<T> {
        self.map_err(|e| DecodeError::Os {
            call,
            code: e.code().0 as u32,
        })
    }
}

/// Inicializa COM (MTA si el hilo aún no tiene apartamento) y Media
/// Foundation una vez por proceso.
pub(crate) fn ensure_mf_started() -> Result<()> {
    // SAFETY: CoInitializeEx sin reservado. Si el hilo ya está en STA (p. ej.
    // winit llama a OleInitialize), devuelve RPC_E_CHANGED_MODE y COM sigue
    // utilizable: los MFT síncronos funcionan en ambos apartamentos.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(DecodeError::Os {
            call: "CoInitializeEx",
            code: hr.0 as u32,
        });
    }
    static MF_START: OnceLock<HRESULT> = OnceLock::new();
    let hr = *MF_START.get_or_init(|| {
        // SAFETY: MFStartup con la versión de las cabeceras, una vez por proceso.
        match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            Ok(()) => HRESULT(0),
            Err(e) => e.code(),
        }
    });
    if hr.is_err() {
        return Err(DecodeError::Os {
            call: "MFStartup",
            code: hr.0 as u32,
        });
    }
    Ok(())
}

/// Crea un dispositivo D3D11 hardware apto para decodificar vídeo y para
/// presentar (BGRA). Activa la protección multihilo, necesaria porque el MFT
/// usa el contexto inmediato.
pub fn create_d3d11_device() -> Result<ID3D11Device> {
    let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: punteros de salida válidos; adaptador por defecto (en este
    // portátil, la única GPU).
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .ctx("D3D11CreateDevice")?;
    let device = device
        .ok_or_else(|| DecodeError::Unexpected("D3D11CreateDevice sin dispositivo".into()))?;
    let mt: ID3D11Multithread = device.cast().ctx("QueryInterface(ID3D11Multithread)")?;
    // SAFETY: llamada simple sobre una interfaz válida.
    unsafe {
        let _ = mt.SetMultithreadProtected(BOOL::from(true));
    }
    Ok(device)
}

/// Frame decodificado que vive en la GPU.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Textura NV12 del decodificador (normalmente un *array* de superficies).
    pub texture: ID3D11Texture2D,
    /// Índice de subrecurso (= índice en el array) del frame dentro de `texture`.
    pub subresource: u32,
    /// Ancho visible en píxeles (apertura de visualización).
    pub width: u32,
    /// Alto visible en píxeles (p. ej. 1080 aunque la superficie tenga 1088).
    pub height: u32,
    /// Marca de tiempo de presentación en microsegundos (la de la entrada).
    pub pts_us: i64,
    /// Tiempo desde que se entregó la AU hasta que el MFT devolvió el frame.
    pub decode_time: Duration,
    /// Tiempo hasta que la GPU terminó de escribir el frame (solo si se
    /// activó [`VideoDecoder::set_measure_gpu`]).
    pub gpu_done_time: Option<Duration>,
    /// Mantiene la superficie fuera del *pool* del MFT mientras viva.
    _sample: IMFSample,
}

// SAFETY: un `DecodedFrame` solo contiene interfaces COM con recuento de
// referencias. Las de D3D11 ya son `Send` en `windows`; el `IMFSample` lo crea
// el MFT de Media Foundation, cuyos objetos (muestras y atributos) son
// *free-threaded* (MTA). El visor lo decodifica en un hilo y lo presenta y
// suelta en el de la ventana; el dispositivo D3D11 compartido tiene la
// protección multihilo activada (`create_d3d11_device`), así que usar la
// textura desde otro hilo queda serializado por el runtime.
unsafe impl Send for DecodedFrame {}

/// Decodificador H.264 por hardware (Media Foundation + D3D11).
#[derive(Debug)]
pub struct VideoDecoder {
    mft: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    context: ID3D11DeviceContext,
    query: ID3D11Query,
    width: u32,
    height: u32,
    /// (pts en unidades de 100 ns, instante de entrada) de AUs pendientes.
    in_flight: VecDeque<(i64, Instant)>,
    ready: VecDeque<DecodedFrame>,
    measure_gpu: bool,
    stream_changes: u32,
}

impl VideoDecoder {
    /// Crea el decodificador sobre `device` (que debe venir de
    /// [`create_d3d11_device`] o tener `VIDEO_SUPPORT` y protección multihilo).
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        ensure_mf_started()?;
        // SAFETY: todas las llamadas COM de este bloque reciben interfaces
        // válidas y punteros de salida locales.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_CMSH264_DECODER_MFT, None, CLSCTX_INPROC_SERVER)
                    .ctx("CoCreateInstance(CMSH264DecoderMFT)")?;

            let attrs = mft.GetAttributes().ctx("IMFTransform::GetAttributes")?;
            let d3d11_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            if !d3d11_aware {
                return Err(DecodeError::Unsupported(
                    "el MFT H.264 no es D3D11-aware".into(),
                ));
            }
            attrs
                .SetUINT32(&MF_LOW_LATENCY, 1)
                .ctx("SetUINT32(MF_LOW_LATENCY)")?;
            if let Ok(codec) = mft.cast::<ICodecAPI>() {
                let v = VARIANT::from(1u32);
                if let Err(e) = codec.SetValue(&CODECAPI_AVLowLatencyMode, &v) {
                    tracing::debug!("CODECAPI_AVLowLatencyMode no aplicado: {e}");
                }
            }

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).ctx("MFCreateDXGIDeviceManager")?;
            let manager = manager.ok_or_else(|| {
                DecodeError::Unexpected("MFCreateDXGIDeviceManager sin gestor".into())
            })?;
            manager
                .ResetDevice(device, token)
                .ctx("IMFDXGIDeviceManager::ResetDevice")?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(|e| {
                    DecodeError::Unsupported(format!(
                        "el MFT rechazó el gestor D3D11 (sin DXVA): 0x{:08X}",
                        e.code().0 as u32
                    ))
                })?;

            // Superficies de salida muestreables por shader: permite crear
            // SRV directamente sobre la textura del decodificador.
            if let Ok(out_attrs) = mft.GetOutputStreamAttributes(0) {
                let flags = (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                if let Err(e) = out_attrs.SetUINT32(&MF_SA_D3D11_BINDFLAGS, flags) {
                    tracing::warn!("MF_SA_D3D11_BINDFLAGS no aplicado: {e}");
                }
            }

            let in_type = MFCreateMediaType().ctx("MFCreateMediaType")?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .ctx("SetGUID(major)")?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .ctx("SetGUID(subtype)")?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ctx("SetUINT32(interlace)")?;
            mft.SetInputType(0, &in_type, 0)
                .ctx("IMFTransform::SetInputType")?;

            let context = device.GetImmediateContext().ctx("GetImmediateContext")?;
            let mut query = None;
            device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .ctx("CreateQuery(EVENT)")?;
            let query = query.ok_or_else(|| DecodeError::Unexpected("sin query".into()))?;

            let mut dec = Self {
                mft,
                _manager: manager,
                context,
                query,
                width: 0,
                height: 0,
                in_flight: VecDeque::new(),
                ready: VecDeque::new(),
                measure_gpu: false,
                stream_changes: 0,
            };
            dec.configure_output()?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .ctx("NOTIFY_BEGIN_STREAMING")?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .ctx("NOTIFY_START_OF_STREAM")?;
            Ok(dec)
        }
    }

    /// Si se activa, tras cada frame se espera (con una *event query*) a que
    /// la GPU haya terminado de decodificarlo y se rellena
    /// [`DecodedFrame::gpu_done_time`]. Serializa CPU y GPU: solo para medir.
    pub fn set_measure_gpu(&mut self, on: bool) {
        self.measure_gpu = on;
    }

    /// Resolución visible actual (0×0 hasta el primer frame).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Número de `MF_E_TRANSFORM_STREAM_CHANGE` procesados.
    pub fn stream_changes(&self) -> u32 {
        self.stream_changes
    }

    /// Entrega una AU Annex-B completa (puede traer SPS/PPS en línea) y
    /// devuelve el frame resultante si el MFT ya lo tiene. Con flujos sin
    /// B-frames y baja latencia, el frame sale en la misma llamada.
    pub fn decode(
        &mut self,
        annexb_access_unit: &[u8],
        pts_us: i64,
    ) -> Result<Option<DecodedFrame>> {
        let sample = make_input_sample(annexb_access_unit, pts_us * 10)?;
        self.in_flight.push_back((pts_us * 10, Instant::now()));
        // Acotar por si el MFT descarta entradas (p. ej. P sin IDR previo).
        while self.in_flight.len() > 64 {
            self.in_flight.pop_front();
        }
        loop {
            // SAFETY: muestra válida creada arriba.
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // El MFT tiene salida pendiente: vaciarla y reintentar.
                    let before = self.ready.len();
                    self.drain_output()?;
                    if self.ready.len() == before {
                        return Err(DecodeError::Unexpected(
                            "MF_E_NOTACCEPTING sin salida disponible".into(),
                        ));
                    }
                }
                Err(e) => {
                    return Err(DecodeError::Os {
                        call: "IMFTransform::ProcessInput",
                        code: e.code().0 as u32,
                    })
                }
            }
        }
        self.drain_output()?;
        Ok(self.ready.pop_front())
    }

    /// Frames ya decodificados que no se devolvieron todavía (si el MFT
    /// entregó más de uno por entrada).
    pub fn next_ready(&mut self) -> Option<DecodedFrame> {
        self.ready.pop_front()
    }

    /// Fin de flujo: pide al MFT los frames retenidos y los devuelve.
    pub fn drain(&mut self) -> Result<Vec<DecodedFrame>> {
        // SAFETY: mensaje sin parámetros.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.ctx("COMMAND_DRAIN")?;
        self.drain_output()?;
        Ok(self.ready.drain(..).collect())
    }

    /// Descarta todo lo pendiente (p. ej. tras una pérdida, antes de un IDR).
    pub fn flush(&mut self) -> Result<()> {
        self.ready.clear();
        self.in_flight.clear();
        // SAFETY: mensaje sin parámetros.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }.ctx("COMMAND_FLUSH")
    }

    /// Llama a `ProcessOutput` hasta `NEED_MORE_INPUT`, encolando frames.
    fn drain_output(&mut self) -> Result<()> {
        loop {
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: un búfer de salida; el MFT aporta la muestra
            // (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES con gestor D3D).
            let r = unsafe {
                self.mft
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status)
            };
            // SAFETY: tomamos la propiedad de lo que el MFT haya dejado en el
            // búfer para liberarlo correctamente, haya ido bien o no.
            let sample = unsafe { ManuallyDrop::take(&mut buf.pSample) };
            // SAFETY: ídem para la colección de eventos.
            drop(unsafe { ManuallyDrop::take(&mut buf.pEvents) });
            match r {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let frame = self.wrap_output(sample)?;
                        self.ready.push_back(frame);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.stream_changes += 1;
                    self.configure_output()?;
                }
                Err(e) => {
                    return Err(DecodeError::Os {
                        call: "IMFTransform::ProcessOutput",
                        code: e.code().0 as u32,
                    })
                }
            }
        }
    }

    /// Elige NV12 entre los tipos de salida disponibles y lee la resolución.
    fn configure_output(&mut self) -> Result<()> {
        let mut i = 0;
        loop {
            // SAFETY: enumeración por índice; termina con MF_E_NO_MORE_TYPES.
            let t = match unsafe { self.mft.GetOutputAvailableType(0, i) } {
                Ok(t) => t,
                Err(e) => {
                    return Err(DecodeError::Os {
                        call: "GetOutputAvailableType(NV12 no encontrado)",
                        code: e.code().0 as u32,
                    })
                }
            };
            // SAFETY: lectura de un atributo del tipo enumerado.
            let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ctx("GetGUID(subtype)")?;
            if sub == MFVideoFormat_NV12 {
                // SAFETY: tipo válido devuelto por el propio MFT.
                unsafe { self.mft.SetOutputType(0, &t, 0) }.ctx("IMFTransform::SetOutputType")?;
                let (w, h) = display_size(&t)?;
                if (w, h) != (self.width, self.height) {
                    tracing::info!("salida del decodificador: NV12 {w}x{h}");
                }
                self.width = w;
                self.height = h;
                return Ok(());
            }
            i += 1;
        }
    }

    fn wrap_output(&mut self, sample: IMFSample) -> Result<DecodedFrame> {
        let now = Instant::now();
        // SAFETY: muestra válida devuelta por el MFT.
        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let start = match self.in_flight.iter().position(|&(p, _)| p == pts) {
            Some(idx) => {
                let (_, t) = self.in_flight[idx];
                // Lo anterior a esta entrada ya no saldrá (o salió sin pts).
                self.in_flight.drain(..=idx);
                t
            }
            None => now,
        };
        let decode_time = now - start;
        // SAFETY: la muestra del MFT D3D11 tiene un búfer con IMFDXGIBuffer.
        let (texture, subresource) = unsafe {
            let buffer = sample.GetBufferByIndex(0).ctx("GetBufferByIndex")?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(|_| {
                DecodeError::Unexpected(
                    "la salida no es una superficie DXGI (¿decodificación por software?)".into(),
                )
            })?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                .ctx("IMFDXGIBuffer::GetResource")?;
            // SAFETY: GetResource devolvió una referencia propia a un
            // ID3D11Texture2D (el IID pedido).
            let tex = ID3D11Texture2D::from_raw(raw);
            let idx = dxgi.GetSubresourceIndex().ctx("GetSubresourceIndex")?;
            (tex, idx)
        };
        let gpu_done_time = if self.measure_gpu {
            self.wait_gpu_idle();
            Some(start.elapsed())
        } else {
            None
        };
        Ok(DecodedFrame {
            texture,
            subresource,
            width: self.width,
            height: self.height,
            pts_us: pts / 10,
            decode_time,
            gpu_done_time,
            _sample: sample,
        })
    }

    /// Espera a que la GPU complete el trabajo encolado hasta ahora en el
    /// contexto inmediato (incluida la decodificación del MFT).
    fn wait_gpu_idle(&self) {
        // SAFETY: query y contexto válidos; `done` es un BOOL local del tamaño
        // que espera D3D11_QUERY_EVENT.
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
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.ready.clear();
        // SAFETY: notificaciones de fin; los errores no importan al destruir.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Tamaño visible: `MF_MT_MINIMUM_DISPLAY_APERTURE` si existe (recorta las
/// 8 líneas de relleno de 1088), si no `MF_MT_FRAME_SIZE`.
fn display_size(t: &IMFMediaType) -> Result<(u32, u32)> {
    // SAFETY: lecturas de atributos sobre un tipo válido; el blob se copia en
    // un búfer local del tamaño de MFVideoArea (16 bytes).
    unsafe {
        let mut area = [0u8; 16];
        let mut len = 0u32;
        if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, Some(&mut len))
            .is_ok()
            && len >= 16
        {
            let cx = u32::from_le_bytes([area[8], area[9], area[10], area[11]]);
            let cy = u32::from_le_bytes([area[12], area[13], area[14], area[15]]);
            if cx > 0 && cy > 0 {
                return Ok((cx, cy));
            }
        }
        let packed = t
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .ctx("GetUINT64(MF_MT_FRAME_SIZE)")?;
        Ok(((packed >> 32) as u32, packed as u32))
    }
}

/// Crea una IMFSample con una copia de `data` y la marca de tiempo dada.
pub(crate) fn make_input_sample(data: &[u8], time_100ns: i64) -> Result<IMFSample> {
    let len = u32::try_from(data.len())
        .map_err(|_| DecodeError::Unexpected("AU demasiado grande".into()))?;
    // SAFETY: el búfer se crea con capacidad `len`; se bloquea, se copian
    // exactamente `len` bytes y se desbloquea antes de usarlo.
    unsafe {
        let buffer = MFCreateMemoryBuffer(len.max(1)).ctx("MFCreateMemoryBuffer")?;
        let mut ptr = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .ctx("IMFMediaBuffer::Lock")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().ctx("IMFMediaBuffer::Unlock")?;
        buffer.SetCurrentLength(len).ctx("SetCurrentLength")?;
        let sample = MFCreateSample().ctx("MFCreateSample")?;
        sample.AddBuffer(&buffer).ctx("AddBuffer")?;
        sample.SetSampleTime(time_100ns).ctx("SetSampleTime")?;
        Ok(sample)
    }
}
