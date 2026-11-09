//! Codificador H.264 por software para el emisor Linux.
//!
//! El binario de OpenH264 se carga desde Cisco en tiempo de ejecución. No se
//! redistribuye con CrossScreen: el usuario lo obtiene con
//! `scripts/fetch-openh264.sh`, igual que en el receptor Linux.

use crate::{
    EncodeControl, EncodeError, EncodedUnit, EncoderCaps, EncoderConfig, EncoderInfo, Result,
    VideoEncoder,
};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig as OpenH264Config, FrameRate, FrameType, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const OPENH264_BLOB: &str = "libopenh264-2.6.0-linux64.8.so";
pub const OPENH264_ENV: &str = "CROSSSCREEN_OPENH264";

/// Frame I420 propio del emisor. Los planos pueden tener stride superior al
/// ancho visible, como ocurre con los buffers entregados por PipeWire.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub strides: (usize, usize, usize),
}

impl I420Frame {
    pub fn new(
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        strides: (usize, usize, usize),
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(EncodeError::BadInput(format!(
                "I420 requiere dimensiones pares no nulas, recibido {width}x{height}"
            )));
        }
        let expected = [
            strides.0.saturating_mul(height as usize),
            strides.1.saturating_mul(height as usize / 2),
            strides.2.saturating_mul(height as usize / 2),
        ];
        if y.len() < expected[0] || u.len() < expected[1] || v.len() < expected[2] {
            return Err(EncodeError::BadInput("planos I420 truncados".into()));
        }
        Ok(Self {
            width,
            height,
            y,
            u,
            v,
            strides,
        })
    }
}

impl YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        self.strides
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

pub struct OpenH264Encoder {
    inner: Encoder,
    caps: EncoderCaps,
    force_idr: bool,
    frame_duration_us: u64,
    next_pts_us: u64,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("caps", &self.caps)
            .field("force_idr", &self.force_idr)
            .field("frame_duration_us", &self.frame_duration_us)
            .field("next_pts_us", &self.next_pts_us)
            .finish_non_exhaustive()
    }
}

impl OpenH264Encoder {
    pub fn from_blob_path(blob: impl AsRef<Path>, cfg: &EncoderConfig) -> Result<Self> {
        let blob = blob.as_ref();
        let api = OpenH264API::from_blob_path(blob)
            .map_err(|e| EncodeError::Unsupported(format!("cargando OpenH264: {e}")))?;
        let open_cfg = OpenH264Config::new()
            .bitrate(BitRate::from_bps(cfg.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(
                cfg.fps_num as f32 / cfg.fps_den.max(1) as f32,
            ))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false);
        let inner = Encoder::with_api_config(api, open_cfg)
            .map_err(|e| EncodeError::Unsupported(format!("creando OpenH264: {e}")))?;
        Ok(Self {
            inner,
            caps: EncoderCaps {
                info: EncoderInfo {
                    friendly_name: "Cisco OpenH264 (software)".into(),
                    vendor_id: None,
                    hardware: false,
                    adapter_luid: None,
                },
                async_mft: false,
                d3d11_aware: false,
                provides_output_samples: false,
                sequence_header_in_type: false,
                properties: Vec::new(),
            },
            force_idr: false,
            frame_duration_us: cfg.frame_duration_us(),
            next_pts_us: 0,
        })
    }

    pub fn from_default_path(cfg: &EncoderConfig) -> Result<Self> {
        locate_blob().and_then(|path| Self::from_blob_path(path, cfg))
    }
}

impl VideoEncoder for OpenH264Encoder {
    type Frame = I420Frame;

    fn encode(&mut self, frame: &Self::Frame, ctl: EncodeControl) -> Result<Vec<EncodedUnit>> {
        if ctl.force_idr {
            self.force_idr = true;
        }
        if self.force_idr {
            self.inner.force_intra_frame();
            self.force_idr = false;
        }
        let start = Instant::now();
        let pts_us = self.next_pts_us;
        self.next_pts_us = self.next_pts_us.saturating_add(self.frame_duration_us);
        let timestamp = Timestamp::from_millis(pts_us / 1_000);
        let stream = self
            .inner
            .encode_at(frame, timestamp)
            .map_err(|e| EncodeError::Unsupported(format!("codificando OpenH264: {e}")))?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let mut data = Vec::new();
        stream.write_vec(&mut data);
        if data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![EncodedUnit {
            data,
            keyframe,
            pts_us,
            encode_time: start.elapsed(),
            hw_time: Duration::ZERO,
        }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedUnit>> {
        Ok(Vec::new())
    }

    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }
}

fn locate_blob() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os(OPENH264_ENV) {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(OPENH264_BLOB));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local/share/crossscreen")
                .join(OPENH264_BLOB),
        );
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            EncodeError::Unsupported(format!(
                "no se encontró {OPENH264_BLOB}; ejecuta scripts/fetch-openh264.sh"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::I420Frame;
    use openh264::formats::YUVSource;

    #[test]
    fn rejects_odd_dimensions() {
        let err = I420Frame::new(3, 2, vec![0; 6], vec![0; 2], vec![0; 2], (3, 1, 1))
            .expect_err("las dimensiones impares deben rechazarse");
        assert!(err.to_string().contains("dimensiones pares"));
    }

    #[test]
    fn accepts_padded_planes() {
        let frame = I420Frame::new(4, 4, vec![0; 24], vec![0; 8], vec![0; 8], (6, 4, 4))
            .expect("los strides pueden superar el ancho visible");
        assert_eq!(frame.dimensions(), (4, 4));
        assert_eq!(frame.strides(), (6, 4, 4));
    }

    #[test]
    fn rejects_truncated_planes() {
        let err = I420Frame::new(4, 4, vec![0; 15], vec![0; 4], vec![0; 4], (4, 2, 2))
            .expect_err("un plano corto debe rechazarse");
        assert!(err.to_string().contains("truncados"));
    }
}
