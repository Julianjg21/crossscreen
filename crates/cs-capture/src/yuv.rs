//! Conversión de RGB empaquetado (lo que entregan los compositores Linux por
//! PipeWire: `BGRx`, `BGRA`, `RGBx`…) a I420 BT.709 de rango limitado, el
//! formato que consume el codificador OpenH264 y que esperan los receptores.

/// Orden de bytes de un píxel empaquetado de 4 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedRgb {
    /// `B G R x` / `B G R A` (el habitual de GNOME/Mutter).
    Bgrx,
    /// `R G B x` / `R G B A`.
    Rgbx,
    /// `x R G B` / `A R G B`.
    Xrgb,
    /// `x B G R` / `A B G R`.
    Xbgr,
}

impl PackedRgb {
    /// Posiciones de R, G y B dentro de los 4 bytes del píxel.
    fn offsets(self) -> (usize, usize, usize) {
        match self {
            PackedRgb::Bgrx => (2, 1, 0),
            PackedRgb::Rgbx => (0, 1, 2),
            PackedRgb::Xrgb => (1, 2, 3),
            PackedRgb::Xbgr => (3, 2, 1),
        }
    }
}

/// Planos I420 con *stride* igual al ancho de cada plano.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Planes {
    /// Luma, `width × height`.
    pub y: Vec<u8>,
    /// Cb, `ceil(width/2) × ceil(height/2)`.
    pub u: Vec<u8>,
    /// Cr, mismo tamaño que `u`.
    pub v: Vec<u8>,
}

/// Convierte una imagen RGB empaquetada (`stride` bytes por fila) a I420.
///
/// Recorre la imagen una sola vez, de dos en dos filas (cada bloque 2×2 da
/// cuatro lumas y una muestra de croma), y reparte los tramos de filas entre
/// varios hilos en imágenes grandes. Devuelve `None` si el búfer no contiene
/// `height` filas completas.
pub fn packed_to_i420(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    order: PackedRgb,
) -> Option<I420Planes> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let row_bytes = w.checked_mul(4)?;
    if stride < row_bytes || src.len() < stride.checked_mul(h - 1)?.checked_add(row_bytes)? {
        return None;
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut y = vec![0u8; w.checked_mul(h)?];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    let job = Job {
        src,
        stride,
        w,
        h,
        offsets: order.offsets(),
    };
    let threads = conversion_threads(w * h).min(ch);
    if threads <= 1 {
        job.rows(0, &mut y, &mut u, &mut v);
    } else {
        // Reparto por filas de croma: cada hilo escribe su tramo de los tres planos.
        let crows = ch.div_ceil(threads);
        std::thread::scope(|scope| {
            let parts = y
                .chunks_mut(2 * w * crows)
                .zip(u.chunks_mut(cw * crows))
                .zip(v.chunks_mut(cw * crows))
                .enumerate();
            for (i, ((yp, up), vp)) in parts {
                let job = &job;
                scope.spawn(move || job.rows(i * crows, yp, up, vp));
            }
        });
    }
    Some(I420Planes { y, u, v })
}

/// Hilos para convertir `pixels` píxeles: uno en imágenes pequeñas y hasta
/// 4 en grandes (deja núcleos libres para el codificador y PipeWire).
fn conversion_threads(pixels: usize) -> usize {
    if pixels < 640 * 480 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 4))
        .unwrap_or(1)
}

/// Datos compartidos (solo lectura) por los hilos de conversión.
struct Job<'a> {
    src: &'a [u8],
    stride: usize,
    w: usize,
    h: usize,
    offsets: (usize, usize, usize),
}

impl Job<'_> {
    /// Convierte las filas de croma desde `first_crow` que caben en `u`; `y`
    /// empieza en la fila de luma `2·first_crow`.
    fn rows(&self, first_crow: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
        let (w, h) = (self.w, self.h);
        let cw = w.div_ceil(2);
        let (ri, gi, bi) = self.offsets;
        let px = |row: &[u8], col: usize| -> (i32, i32, i32) {
            let p = &row[col * 4..col * 4 + 4];
            (i32::from(p[ri]), i32::from(p[gi]), i32::from(p[bi]))
        };
        // Coeficientes BT.709 de rango limitado en punto fijo (×256).
        let luma =
            |(r, g, b): (i32, i32, i32)| (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        for (k, (urow, vrow)) in u.chunks_mut(cw).zip(v.chunks_mut(cw)).enumerate() {
            let r0 = 2 * (first_crow + k);
            let rows: [Option<&[u8]>; 2] = [r0, r0 + 1]
                .map(|r| (r < h).then(|| &self.src[r * self.stride..r * self.stride + w * 4]));
            for (cc, (uo, vo)) in urow.iter_mut().zip(vrow.iter_mut()).enumerate() {
                let (mut sr, mut sg, mut sb, mut n) = (0, 0, 0, 0);
                for (dr, row) in rows.iter().enumerate() {
                    let Some(row) = row else { continue };
                    let (c0, c1) = (2 * cc, (2 * cc + 2).min(w));
                    let yrow = &mut y[(2 * k + dr) * w + c0..(2 * k + dr) * w + c1];
                    for (col, yo) in (c0..c1).zip(yrow.iter_mut()) {
                        let p = px(row, col);
                        *yo = luma(p);
                        sr += p.0;
                        sg += p.1;
                        sb += p.2;
                        n += 1;
                    }
                }
                // Media del bloque 2×2 (recortado en bordes impares).
                let (r, g, b) = (sr / n, sg / n, sb / n);
                let cb = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
                let cr = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
                *uo = cb.clamp(0, 255) as u8;
                *vo = cr.clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{packed_to_i420, I420Planes, PackedRgb};

    /// Implementación anterior (dos pasadas, un hilo), como referencia exacta.
    fn reference(src: &[u8], stride: usize, w: usize, h: usize, order: PackedRgb) -> I420Planes {
        let (ri, gi, bi) = order.offsets();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];
        let rgb = |row: usize, col: usize| -> (i32, i32, i32) {
            let p = &src[row * stride + col * 4..];
            (i32::from(p[ri]), i32::from(p[gi]), i32::from(p[bi]))
        };
        for row in 0..h {
            for col in 0..w {
                let (r, g, b) = rgb(row, col);
                y[row * w + col] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
            }
        }
        for crow in 0..ch {
            for ccol in 0..cw {
                let (mut sr, mut sg, mut sb, mut n) = (0, 0, 0, 0);
                for row in (crow * 2)..((crow * 2 + 2).min(h)) {
                    for col in (ccol * 2)..((ccol * 2 + 2).min(w)) {
                        let (r, g, b) = rgb(row, col);
                        sr += r;
                        sg += g;
                        sb += b;
                        n += 1;
                    }
                }
                let (r, g, b) = (sr / n, sg / n, sb / n);
                let cb = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
                let cr = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
                u[crow * cw + ccol] = cb.clamp(0, 255) as u8;
                v[crow * cw + ccol] = cr.clamp(0, 255) as u8;
            }
        }
        I420Planes { y, u, v }
    }

    #[test]
    fn matches_reference_on_random_images() {
        // Pseudoaleatorio determinista (xorshift): tamaños pares e impares, con
        // relleno de stride y lo bastante grandes para usar varios hilos.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        };
        let orders = [
            PackedRgb::Bgrx,
            PackedRgb::Rgbx,
            PackedRgb::Xrgb,
            PackedRgb::Xbgr,
        ];
        let sizes = [
            (1, 1, 0),
            (2, 2, 0),
            (3, 5, 4),
            (17, 9, 12),
            (1366, 768, 0),
            (1365, 767, 8),
            (800, 601, 16),
        ];
        for (i, (w, h, pad)) in sizes.into_iter().enumerate() {
            let stride = w * 4 + pad;
            let src: Vec<u8> = (0..stride * h).map(|_| next()).collect();
            let order = orders[i % orders.len()];
            let got = packed_to_i420(&src, stride, w as u32, h as u32, order).unwrap();
            assert_eq!(
                got,
                reference(&src, stride, w, h, order),
                "{w}x{h} relleno {pad}"
            );
        }
    }

    fn solid(px: [u8; 4], w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut buf = vec![0xEEu8; stride * h];
        for row in 0..h {
            for col in 0..w {
                buf[row * stride + col * 4..][..4].copy_from_slice(&px);
            }
        }
        buf
    }

    #[test]
    fn black_white_and_primaries_bt709_limited() {
        let conv = |px, order| packed_to_i420(&solid(px, 2, 2, 8), 8, 2, 2, order).unwrap();
        let black = conv([0, 0, 0, 255], PackedRgb::Bgrx);
        assert_eq!((black.y[0], black.u[0], black.v[0]), (16, 128, 128));
        let white = conv([255, 255, 255, 255], PackedRgb::Bgrx);
        assert_eq!((white.y[0], white.u[0], white.v[0]), (235, 127, 128));
        // Rojo puro en BGRx (B,G,R,x) y en RGBx deben coincidir.
        let red_bgrx = conv([0, 0, 255, 0], PackedRgb::Bgrx);
        let red_rgbx = conv([255, 0, 0, 0], PackedRgb::Rgbx);
        assert_eq!(red_bgrx, red_rgbx);
        // BT.709: rojo → Y≈63, Cr alto.
        assert_eq!(red_bgrx.y[0], 63);
        assert!(red_bgrx.v[0] > 230);
    }

    #[test]
    fn stride_padding_is_ignored_and_odd_sizes_work() {
        // 3×3 con 4 bytes de relleno por fila; el relleno (0xEE) no debe influir.
        let buf = solid([0, 0, 0, 0], 3, 3, 16);
        let out = packed_to_i420(&buf, 16, 3, 3, PackedRgb::Xrgb).unwrap();
        assert_eq!(out.y, vec![16; 9]);
        assert_eq!((out.u.len(), out.v.len()), (4, 4));
        assert!(out.u.iter().chain(&out.v).all(|&c| c == 128));
    }

    #[test]
    fn rejects_short_or_inconsistent_buffers() {
        assert!(packed_to_i420(&[0; 15], 8, 2, 2, PackedRgb::Bgrx).is_none());
        assert!(packed_to_i420(&[0; 64], 4, 2, 2, PackedRgb::Bgrx).is_none()); // stride < ancho
        assert!(packed_to_i420(&[], 0, 0, 0, PackedRgb::Bgrx).is_none());
    }
}
