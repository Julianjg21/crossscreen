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
    /// Entrada remota (F6, KVM): del receptor hacia el emisor, que la inyecta
    /// acotada al monitor transmitido.
    Input(InputEvent),
}

/// Botón del ratón.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// Botón lateral "atrás" (XBUTTON1 en Windows).
    Back,
    /// Botón lateral "adelante" (XBUTTON2 en Windows).
    Forward,
}

/// Estado de un botón o tecla.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Evento de entrada remota.
///
/// Las coordenadas del ratón son **absolutas y normalizadas** al monitor del
/// flujo (`0..=65535` en cada eje): no dependen de la resolución del receptor
/// y no acumulan error como el movimiento relativo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        stream_id: StreamId,
        x_norm: u16,
        y_norm: u16,
    },
    MouseButton {
        stream_id: StreamId,
        button: MouseButton,
        state: ButtonState,
    },
    /// Rueda en unidades de Windows: 120 = un escalón estándar. Positivo en
    /// `delta_y` = alejar del usuario (arriba); positivo en `delta_x` = derecha.
    MouseWheel {
        stream_id: StreamId,
        delta_x: i16,
        delta_y: i16,
    },
    /// Tecla por *scan code* (conjunto 1, el de Windows), independiente de la
    /// distribución de teclado. `extended` = prefijo E0 (flechas, Supr, Ctrl
    /// derecho, etc.).
    Key {
        stream_id: StreamId,
        scancode: u16,
        extended: bool,
        state: ButtonState,
    },
}

impl InputEvent {
    pub fn stream_id(&self) -> StreamId {
        match self {
            InputEvent::MouseMove { stream_id, .. }
            | InputEvent::MouseButton { stream_id, .. }
            | InputEvent::MouseWheel { stream_id, .. }
            | InputEvent::Key { stream_id, .. } => *stream_id,
        }
    }
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

    use crate::framing::{encode, FrameDecoder};
    use proptest::prelude::*;

    fn roundtrip(msg: &ControlMessage) -> ControlMessage {
        let wire = encode(msg).expect("serializa");
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        dec.next_frame::<ControlMessage>()
            .expect("deserializa")
            .expect("mensaje completo")
    }

    #[test]
    fn input_events_roundtrip() {
        let events = [
            InputEvent::MouseMove {
                stream_id: 1,
                x_norm: 0,
                y_norm: u16::MAX,
            },
            InputEvent::MouseButton {
                stream_id: 1,
                button: MouseButton::Forward,
                state: ButtonState::Pressed,
            },
            InputEvent::MouseWheel {
                stream_id: 1,
                delta_x: -120,
                delta_y: 240,
            },
            InputEvent::Key {
                stream_id: 1,
                scancode: 0x4B,
                extended: true,
                state: ButtonState::Released,
            },
        ];
        for ev in events {
            let msg = ControlMessage::Input(ev.clone());
            assert_eq!(roundtrip(&msg), msg);
            assert_eq!(ev.stream_id(), 1);
        }
    }

    #[test]
    fn input_messages_are_compact() {
        // El movimiento del ratón se envía con mucha frecuencia: debe ser pequeño.
        let msg = ControlMessage::Input(InputEvent::MouseMove {
            stream_id: 1,
            x_norm: 65535,
            y_norm: 65535,
        });
        assert!(encode(&msg).unwrap().len() <= 16);
    }

    fn arb_button() -> impl Strategy<Value = MouseButton> {
        prop_oneof![
            Just(MouseButton::Left),
            Just(MouseButton::Right),
            Just(MouseButton::Middle),
            Just(MouseButton::Back),
            Just(MouseButton::Forward),
        ]
    }

    fn arb_state() -> impl Strategy<Value = ButtonState> {
        prop_oneof![Just(ButtonState::Pressed), Just(ButtonState::Released)]
    }

    fn arb_input() -> impl Strategy<Value = InputEvent> {
        prop_oneof![
            (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(stream_id, x_norm, y_norm)| {
                InputEvent::MouseMove {
                    stream_id,
                    x_norm,
                    y_norm,
                }
            }),
            (any::<u16>(), arb_button(), arb_state()).prop_map(|(stream_id, button, state)| {
                InputEvent::MouseButton {
                    stream_id,
                    button,
                    state,
                }
            }),
            (any::<u16>(), any::<i16>(), any::<i16>()).prop_map(|(stream_id, delta_x, delta_y)| {
                InputEvent::MouseWheel {
                    stream_id,
                    delta_x,
                    delta_y,
                }
            }),
            (any::<u16>(), any::<u16>(), any::<bool>(), arb_state()).prop_map(
                |(stream_id, scancode, extended, state)| InputEvent::Key {
                    stream_id,
                    scancode,
                    extended,
                    state,
                }
            ),
        ]
    }

    proptest! {
        #[test]
        fn arbitrary_input_roundtrip(ev in arb_input()) {
            let msg = ControlMessage::Input(ev);
            prop_assert_eq!(roundtrip(&msg), msg);
        }
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
