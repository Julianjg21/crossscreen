//! Cursor fuera de banda.
//!
//! El cursor no va incrustado en la imagen capturada: se envía aparte
//! (posición a alta frecuencia, forma solo cuando cambia) y el receptor lo
//! dibuja localmente. Así moverlo no obliga a codificar un frame nuevo.

/// Tipo de forma del cursor (mismos significados que DXGI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShapeKind {
    /// 1 bpp: máscara AND (mitad superior) + XOR (mitad inferior);
    /// `height` incluye las dos mitades.
    Monochrome,
    /// 32 bpp BGRA con alfa premultiplicado no garantizado (alfa normal).
    Color,
    /// 32 bpp BGRX; el byte alto actúa de máscara: 0 = sustituir el píxel,
    /// 0xFF = XOR con la pantalla.
    MaskedColor,
}

/// Forma del cursor tal como la entrega el SO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub kind: CursorShapeKind,
    pub width: u32,
    /// Alto del buffer (en `Monochrome` es el doble del alto visible).
    pub height: u32,
    /// Bytes por fila.
    pub pitch: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub data: Vec<u8>,
}

impl CursorShape {
    /// Alto visible del cursor.
    pub fn visible_height(&self) -> u32 {
        match self.kind {
            CursorShapeKind::Monochrome => self.height / 2,
            _ => self.height,
        }
    }

    /// Convierte la forma a RGBA8 sin premultiplicar (`width × visible_height`).
    ///
    /// Los píxeles "invertir pantalla" (XOR) no tienen equivalente en RGBA:
    /// se aproximan a negro opaco. Para dibujarlos exactos el receptor debe
    /// usar `data` con una mezcla XOR. Devuelve `None` si el buffer es más
    /// corto de lo que anuncian `pitch × height`.
    pub fn to_rgba(&self) -> Option<Vec<u8>> {
        let w = self.width as usize;
        let h = self.visible_height() as usize;
        let pitch = self.pitch as usize;
        let mut out = vec![0u8; w * h * 4];
        match self.kind {
            CursorShapeKind::Monochrome => {
                if w.div_ceil(8) > pitch || self.data.len() < pitch * h * 2 {
                    return None;
                }
                for y in 0..h {
                    for x in 0..w {
                        let byte = x / 8;
                        let bit = 0x80u8 >> (x % 8);
                        let and = self.data[y * pitch + byte] & bit != 0;
                        let xor = self.data[(y + h) * pitch + byte] & bit != 0;
                        let px = match (and, xor) {
                            (true, false) => [0, 0, 0, 0],         // transparente
                            (false, false) => [0, 0, 0, 255],      // negro
                            (false, true) => [255, 255, 255, 255], // blanco
                            (true, true) => [0, 0, 0, 255],        // invertir (aprox.)
                        };
                        out[(y * w + x) * 4..][..4].copy_from_slice(&px);
                    }
                }
            }
            CursorShapeKind::Color | CursorShapeKind::MaskedColor => {
                if w * 4 > pitch || self.data.len() < pitch * h {
                    return None;
                }
                for y in 0..h {
                    for x in 0..w {
                        let s = &self.data[y * pitch + x * 4..][..4];
                        let (b, g, r, a) = (s[0], s[1], s[2], s[3]);
                        let px = if self.kind == CursorShapeKind::Color {
                            [r, g, b, a]
                        } else if a == 0 {
                            [r, g, b, 255] // sustituir
                        } else if r == 0 && g == 0 && b == 0 {
                            [0, 0, 0, 0] // XOR con 0: no cambia la pantalla
                        } else {
                            [0, 0, 0, 255] // XOR con color: invertir (aprox.)
                        };
                        out[(y * w + x) * 4..][..4].copy_from_slice(&px);
                    }
                }
            }
        }
        Some(out)
    }
}

/// Último estado conocido del cursor sobre la salida capturada.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    /// Posición de la esquina superior izquierda de la forma, relativa a la
    /// salida (ya incluye el *hotspot*). `None` hasta el primer dato.
    pub position: Option<(i32, i32)>,
    /// ¿Está el cursor sobre esta salida y visible?
    pub visible: bool,
    pub shape: Option<CursorShape>,
    /// Se incrementa con cada cambio de posición o visibilidad.
    pub position_generation: u64,
    /// Se incrementa con cada forma nueva.
    pub shape_generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_decodes_four_cases() {
        // 4×1 visible: pitch 1, AND = 1010, XOR = 0110 (bits altos primero).
        let shape = CursorShape {
            kind: CursorShapeKind::Monochrome,
            width: 4,
            height: 2,
            pitch: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            data: vec![0b1010_0000, 0b0110_0000],
        };
        let rgba = shape.to_rgba().unwrap();
        assert_eq!(rgba.len(), 16);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]); // AND1 XOR0 transparente
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]); // AND0 XOR1 blanco
        assert_eq!(&rgba[8..12], &[0, 0, 0, 255]); // AND1 XOR1 invertir
        assert_eq!(&rgba[12..16], &[0, 0, 0, 255]); // AND0 XOR0 negro
    }

    #[test]
    fn color_swaps_channels_and_respects_pitch() {
        // 1×2 con pitch 8 (relleno de 4 bytes por fila).
        let shape = CursorShape {
            kind: CursorShapeKind::Color,
            width: 1,
            height: 2,
            pitch: 8,
            hotspot_x: 0,
            hotspot_y: 0,
            data: vec![1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9, 9],
        };
        assert_eq!(shape.to_rgba().unwrap(), vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn masked_color_rules() {
        let shape = CursorShape {
            kind: CursorShapeKind::MaskedColor,
            width: 3,
            height: 1,
            pitch: 12,
            hotspot_x: 0,
            hotspot_y: 0,
            data: vec![10, 20, 30, 0, 0, 0, 0, 255, 1, 1, 1, 255],
        };
        assert_eq!(
            shape.to_rgba().unwrap(),
            vec![30, 20, 10, 255, 0, 0, 0, 0, 0, 0, 0, 255]
        );
    }

    #[test]
    fn short_buffers_are_rejected() {
        let shape = CursorShape {
            kind: CursorShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            hotspot_x: 0,
            hotspot_y: 0,
            data: vec![0; 15],
        };
        assert!(shape.to_rgba().is_none());
    }
}
