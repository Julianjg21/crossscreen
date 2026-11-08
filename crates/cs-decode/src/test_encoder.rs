//! Herramienta de desarrollo: codificador H.264 por software de Media
//! Foundation (`CLSID_MSH264EncoderMFT`) para generar flujos de prueba
//! Annex-B sin depender de ffmpeg ni de otros agentes.
//!
//! **No es el codificador de producción** (ese vive en `cs-encode`, con MFT
//! hardware y captura). Aquí solo se necesita un flujo H.264 válido y
//! reproducible: imágenes sintéticas NV12 → Annex-B.
#![allow(unsafe_code)] // FFI COM/Media Foundation; cada bloque unsafe está acotado y comentado.

use crate::windows_mf::{ensure_mf_started, make_input_sample, HrContext};
use crate::{DecodeError, Result};
use std::mem::ManuallyDrop;
use windows::core::{Interface, VARIANT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// Codificador H.264 de prueba (MFT síncrono por software).
#[derive(Debug)]
pub struct TestH264Encoder {
    mft: IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    frame_index: i64,
    sequence_header: Vec<u8>,
    sent_header: bool,
}

impl TestH264Encoder {
    /// Crea el codificador: perfil Main, sin B-frames, un IDR cada `gop`
    /// frames, `bitrate` en bit/s.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, gop: u32) -> Result<Self> {
        ensure_mf_started()?;
        // SAFETY: llamadas COM con interfaces válidas y valores locales.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .ctx("CoCreateInstance(MSH264EncoderMFT)")?;
            if let Ok(codec) = mft.cast::<ICodecAPI>() {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ =
                    codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
            }
            let size = ((width as u64) << 32) | height as u64;
            let rate = ((fps as u64) << 32) | 1;

            let out = MFCreateMediaType().ctx("MFCreateMediaType")?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .ctx("major")?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .ctx("subtype")?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).ctx("bitrate")?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size).ctx("frame size")?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate).ctx("frame rate")?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .ctx("par")?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ctx("interlace")?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
                .ctx("profile")?;
            mft.SetOutputType(0, &out, 0).ctx("encoder SetOutputType")?;

            let inp = MFCreateMediaType().ctx("MFCreateMediaType")?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .ctx("major")?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .ctx("subtype")?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, size).ctx("frame size")?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, rate).ctx("frame rate")?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .ctx("par")?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ctx("interlace")?;
            mft.SetInputType(0, &inp, 0).ctx("encoder SetInputType")?;

            // SPS/PPS por si el MFT no los repite dentro del primer IDR.
            let mut sequence_header = Vec::new();
            if let Ok(cur) = mft.GetOutputCurrentType(0) {
                if let Ok(n) = cur.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    sequence_header = vec![0u8; n as usize];
                    let mut got = 0u32;
                    if cur
                        .GetBlob(
                            &MF_MT_MPEG_SEQUENCE_HEADER,
                            &mut sequence_header,
                            Some(&mut got),
                        )
                        .is_err()
                    {
                        sequence_header.clear();
                    } else {
                        sequence_header.truncate(got as usize);
                    }
                }
            }

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .ctx("BEGIN_STREAMING")?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .ctx("START_OF_STREAM")?;
            Ok(Self {
                mft,
                width,
                height,
                fps,
                frame_index: 0,
                sequence_header,
                sent_header: false,
            })
        }
    }

    /// Codifica un frame NV12 (`width*height*3/2` bytes, *stride* = ancho) y
    /// devuelve los bytes Annex-B que el MFT haya producido (puede ser vacío
    /// o contener varias AUs).
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<u8>> {
        let expected = (self.width * self.height * 3 / 2) as usize;
        if nv12.len() != expected {
            return Err(DecodeError::Unexpected(format!(
                "frame NV12 de {} bytes, se esperaban {expected}",
                nv12.len()
            )));
        }
        let t = self.frame_index * 10_000_000 / self.fps as i64;
        let sample = make_input_sample(nv12, t)?;
        // SAFETY: muestra válida; duración en unidades de 100 ns.
        unsafe {
            sample
                .SetSampleDuration(10_000_000 / self.fps as i64)
                .ctx("SetSampleDuration")?;
            self.mft
                .ProcessInput(0, &sample, 0)
                .ctx("encoder ProcessInput")?;
        }
        self.frame_index += 1;
        self.collect()
    }

    /// Fin de flujo: devuelve lo que quede en el MFT.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        // SAFETY: mensaje sin parámetros.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.ctx("encoder DRAIN")?;
        self.collect()
    }

    fn collect(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        // SAFETY: consulta de la información del flujo de salida.
        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.ctx("GetOutputStreamInfo")?;
        let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        loop {
            let own_sample = if provides {
                None
            } else {
                // SAFETY: muestra con un búfer del tamaño que pide el MFT.
                unsafe {
                    let s = MFCreateSample().ctx("MFCreateSample")?;
                    let b = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))
                        .ctx("MFCreateMemoryBuffer")?;
                    s.AddBuffer(&b).ctx("AddBuffer")?;
                    Some(s)
                }
            };
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: un búfer de salida inicializado arriba.
            let r = unsafe {
                self.mft
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status)
            };
            // SAFETY: recuperamos la propiedad de muestra y eventos.
            let sample = unsafe { ManuallyDrop::take(&mut buf.pSample) };
            // SAFETY: ídem.
            drop(unsafe { ManuallyDrop::take(&mut buf.pEvents) });
            match r {
                Ok(()) => {
                    if let Some(s) = sample {
                        let bytes = sample_bytes(&s)?;
                        if !self.sent_header {
                            self.sent_header = true;
                            let types = crate::annexb::nal_types(&bytes);
                            if !types.contains(&crate::annexb::nal_type::SPS) {
                                out.extend_from_slice(&self.sequence_header);
                            }
                        }
                        out.extend_from_slice(&bytes);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(out),
                Err(e) => {
                    return Err(DecodeError::Os {
                        call: "encoder ProcessOutput",
                        code: e.code().0 as u32,
                    })
                }
            }
        }
    }
}

fn sample_bytes(s: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: búfer contiguo bloqueado mientras se copia `len` bytes.
    unsafe {
        let b = s
            .ConvertToContiguousBuffer()
            .ctx("ConvertToContiguousBuffer")?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        b.Lock(&mut ptr, None, Some(&mut len)).ctx("Lock")?;
        let v = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        b.Unlock().ctx("Unlock")?;
        Ok(v)
    }
}

/// Imagen sintética NV12 para el frame `n`: barras de color fijas arriba,
/// degradado horizontal abajo y un cuadrado blanco que se desplaza (permite
/// comprobar a ojo que el vídeo avanza y que los colores son correctos).
pub fn synthetic_nv12(width: u32, height: u32, n: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![0u8; w * h * 3 / 2];
    // Barras BT.709 rango limitado: blanco, amarillo, cian, verde, magenta,
    // rojo, azul, negro (Y, U, V).
    const BARS: [(u8, u8, u8); 8] = [
        (235, 128, 128),
        (219, 16, 138),
        (188, 154, 16),
        (173, 42, 26),
        (78, 214, 230),
        (63, 102, 240),
        (32, 240, 118),
        (16, 128, 128),
    ];
    let box_size = h / 6;
    let box_x = (n as usize * 8) % (w - box_size);
    let box_y = h / 2 - box_size / 2;
    let (y_plane, uv_plane) = buf.split_at_mut(w * h);
    for y in 0..h {
        for x in 0..w {
            let in_box = x >= box_x && x < box_x + box_size && y >= box_y && y < box_y + box_size;
            let luma = if in_box {
                235
            } else if y < h / 2 {
                BARS[x * 8 / w].0
            } else {
                (16 + x * 219 / w) as u8
            };
            y_plane[y * w + x] = luma;
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (x, y) = (cx * 2, cy * 2);
            let in_box = x >= box_x && x < box_x + box_size && y >= box_y && y < box_y + box_size;
            let (u, v) = if in_box || y >= h / 2 {
                (128, 128)
            } else {
                let b = BARS[x * 8 / w];
                (b.1, b.2)
            };
            uv_plane[cy * w + cx * 2] = u;
            uv_plane[cy * w + cx * 2 + 1] = v;
        }
    }
    buf
}
