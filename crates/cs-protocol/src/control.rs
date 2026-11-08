//! Mensajes del canal de control.

use crate::ProtocolVersion;
use cs_core::{Codec, CodecSupport, DeviceId, Mode, Roles, StreamMode};
use serde::{Deserialize, Serialize};

/// Identificador de un flujo de pantalla dentro de una sesión.
pub type StreamId = u16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Primer mensaje en ambos sentidos.
    Hello(Hello),
    /// El receptor anuncia qué puede decodificar y su pantalla nativa.
    SinkCapabilities(SinkCapabilities),
    /// La fuente anuncia qué puede codificar y si puede crear monitores virtuales.
    SourceCapabilities(SourceCapabilities),
    /// La fuente (o el receptor, pidiéndolo) propone iniciar un flujo.
    StartStream(StartStream),
    StreamStarted {
        stream_id: StreamId,
    },
    StreamFailed {
        stream_id: StreamId,
        reason: String,
    },
    StopStream {
        stream_id: StreamId,
    },
    /// El receptor pide un frame clave (tras pérdida irrecuperable o al reanudar).
    RequestKeyframe {
        stream_id: StreamId,
    },
    /// El receptor informa de frames perdidos tras aplicar FEC.
    LossReport(LossReport),
    /// Estadísticas periódicas del receptor (cada ~100 ms).
    ReceiverStats(ReceiverStats),
    /// La fuente cambia el bitrate objetivo (informativo para el receptor).
    BitrateChanged {
        stream_id: StreamId,
        bitrate_kbps: u32,
    },
    /// Medición de RTT y estimación de reloj (estilo NTP).
    Ping {
        nonce: u64,
        sent_us: u64,
    },
    Pong {
        nonce: u64,
        ping_sent_us: u64,
        received_us: u64,
    },
    /// Cierre ordenado.
    Bye {
        reason: ByeReason,
    },
    /// Posición del cursor (fuera de banda; ver docs/02-arquitectura.md §2.4).
    /// Coordenadas en píxeles del monitor transmitido; `visible = false` si el
    /// cursor está oculto o fuera de ese monitor.
    CursorPosition {
        stream_id: StreamId,
        x: i32,
        y: i32,
        visible: bool,
    },
    /// Forma del cursor, solo cuando cambia.
    CursorShape(CursorShape),
}

/// Forma del cursor en RGBA8 no premultiplicado, fila a fila sin relleno.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    pub stream_id: StreamId,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl CursorShape {
    /// Límite de tamaño: 128×128 (64 KiB) cabe holgado en un mensaje de control.
    pub const MAX_DIM: u16 = 128;

    pub fn validate(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= Self::MAX_DIM
            && self.height <= Self::MAX_DIM
            && self.hotspot_x < self.width
            && self.hotspot_y < self.height
            && self.rgba.len() == usize::from(self.width) * usize::from(self.height) * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shape_validation() {
        let ok = CursorShape {
            stream_id: 1,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 1,
            rgba: vec![0; 16],
        };
        assert!(ok.validate());
        assert!(!CursorShape {
            rgba: vec![0; 15],
            ..ok.clone()
        }
        .validate());
        assert!(!CursorShape {
            hotspot_x: 2,
            ..ok.clone()
        }
        .validate());
        assert!(!CursorShape {
            width: 200,
            rgba: vec![0; 200 * 2 * 4],
            ..ok
        }
        .validate());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: ProtocolVersion,
    pub device_id: DeviceId,
    pub device_name: String,
    pub roles: Roles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkCapabilities {
    pub decoders: Vec<CodecSupport>,
    pub native_mode: Mode,
    /// DPI de la pantalla del receptor, para sugerir escalado en la fuente.
    pub dpi: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCapabilities {
    pub encoders: Vec<CodecSupport>,
    /// `true` solo si hay un backend de monitor virtual operativo en la fuente.
    pub virtual_display: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartStream {
    pub stream_id: StreamId,
    pub mode: StreamMode,
    pub codec: Codec,
    pub display_mode: Mode,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossReport {
    pub stream_id: StreamId,
    /// Secuencias de frame que no pudieron reconstruirse.
    pub lost_frames: Vec<u32>,
    /// Último frame entregado correctamente al decodificador.
    pub last_good_frame: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverStats {
    pub stream_id: StreamId,
    pub frames_received: u32,
    pub frames_lost: u32,
    /// Fragmentos perdidos antes de FEC, en partes por diez mil.
    pub pre_fec_loss_bp: u16,
    pub jitter_us: u32,
    pub decode_time_us: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByeReason {
    UserRequested,
    Shutdown,
    IncompatibleVersion,
    Error,
}
