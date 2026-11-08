//! Conversión del cursor de la captura (`cs-capture`) a los mensajes del
//! canal de control (`cs-protocol`).
//!
//! Convención de posición (ver `docs/poc-results/mvp-f2-emisor.md`):
//! DXGI entrega la esquina superior izquierda de la imagen del cursor;
//! `CursorPosition` lleva **el punto del *hotspot*** (esquina + *hotspot*),
//! que es lo que el usuario entiende por "posición del ratón". El receptor
//! dibuja la forma en `(x - hotspot_x, y - hotspot_y)`.

use cs_capture::CursorShape as CapShape;
use cs_protocol::control::{CursorShape, StreamId};

/// Lado máximo de la forma enviada (contrato: 128×128).
pub const MAX_DIM: u32 = CursorShape::MAX_DIM as u32;

/// Convierte la forma capturada a RGBA, recortada a 128×128 como máximo.
/// `None` si el búfer es incoherente o la forma está vacía.
pub fn shape_to_message(shape: &CapShape, stream_id: StreamId) -> Option<CursorShape> {
    let rgba = shape.to_rgba()?;
    let (w, h) = (shape.width, shape.visible_height());
    if w == 0 || h == 0 {
        return None;
    }
    let (cw, ch) = (w.min(MAX_DIM), h.min(MAX_DIM));
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in 0..ch as usize {
        let row = y * w as usize * 4;
        out.extend_from_slice(&rgba[row..row + cw as usize * 4]);
    }
    let clamp = |v: i32, max: u32| v.clamp(0, max as i32 - 1) as u16;
    let msg = CursorShape {
        stream_id,
        width: cw as u16,
        height: ch as u16,
        hotspot_x: clamp(shape.hotspot_x, cw),
        hotspot_y: clamp(shape.hotspot_y, ch),
        rgba: out,
    };
    msg.validate().then_some(msg)
}

/// Posición que se envía: el *hotspot* en coordenadas del monitor.
pub fn hotspot_position(top_left: (i32, i32), shape: Option<&CapShape>) -> (i32, i32) {
    let (hx, hy) = shape.map_or((0, 0), |s| (s.hotspot_x, s.hotspot_y));
    (top_left.0 + hx, top_left.1 + hy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_capture::CursorShapeKind;

    fn color(w: u32, h: u32, hx: i32, hy: i32) -> CapShape {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for (i, px) in data.chunks_mut(4).enumerate() {
            px.copy_from_slice(&[1, 2, 3, (i % 251) as u8]); // BGRA
        }
        CapShape {
            kind: CursorShapeKind::Color,
            width: w,
            height: h,
            pitch: w * 4,
            hotspot_x: hx,
            hotspot_y: hy,
            data,
        }
    }

    #[test]
    fn small_shape_is_converted_to_rgba() {
        let m = shape_to_message(&color(32, 32, 3, 4), 1).unwrap();
        assert_eq!(
            (m.width, m.height, m.hotspot_x, m.hotspot_y),
            (32, 32, 3, 4)
        );
        assert_eq!(&m.rgba[..4], &[3, 2, 1, 0]); // BGRA → RGBA
        assert!(m.validate());
    }

    #[test]
    fn large_shape_is_cropped_and_hotspot_clamped() {
        let m = shape_to_message(&color(256, 200, 250, -5), 1).unwrap();
        assert_eq!((m.width, m.height), (128, 128));
        assert_eq!((m.hotspot_x, m.hotspot_y), (127, 0));
        assert_eq!(m.rgba.len(), 128 * 128 * 4);
        // La fila 1 recortada empieza en el píxel 256 del original.
        assert_eq!(m.rgba[128 * 4 + 3], (256 % 251) as u8);
    }

    #[test]
    fn broken_buffer_is_rejected() {
        let mut s = color(32, 32, 0, 0);
        s.data.truncate(10);
        assert!(shape_to_message(&s, 1).is_none());
    }

    #[test]
    fn position_adds_hotspot() {
        let s = color(32, 32, 5, 7);
        assert_eq!(hotspot_position((100, 200), Some(&s)), (105, 207));
        assert_eq!(hotspot_position((100, 200), None), (100, 200));
    }
}
