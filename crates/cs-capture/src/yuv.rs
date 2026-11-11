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

/// ¿Contiene `src` `height` filas completas de `width` píxeles de 4 bytes
/// con este `stride`?
#[cfg_attr(windows, allow(dead_code))] // solo lo usa `yuv_pool` (Linux)
pub(crate) fn packed_fits(src_len: usize, stride: usize, width: u32, height: u32) -> bool {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return false;
    }
    let Some(row_bytes) = w.checked_mul(4) else {
        return false;
    };
    stride >= row_bytes
        && stride
            .checked_mul(h - 1)
            .and_then(|n| n.checked_add(row_bytes))
            .is_some_and(|need| need <= src_len)
}

/// Convierte las filas de croma desde `first_crow` que caben en `u`
/// (`y` empieza en la fila de luma `2·first_crow`). Para repartir una
/// conversión entre hilos propios (ver `yuv_pool`). `src` ya validado con
/// [`packed_fits`].
#[allow(clippy::too_many_arguments)] // los planos van separados para poder trocearlos
#[cfg_attr(windows, allow(dead_code))] // solo lo usa `yuv_pool` (Linux)
pub(crate) fn convert_rows(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    order: PackedRgb,
    first_crow: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let job = Job {
        src,
        stride,
        w: width as usize,
        h: height as usize,
        offsets: order.offsets(),
    };
    job.rows(first_crow, y, u, v);
}

/// Hilos para convertir `pixels` píxeles: uno en imágenes pequeñas y hasta
/// 4 en grandes (deja núcleos libres para el codificador y PipeWire).
pub(crate) fn conversion_threads(pixels: usize) -> usize {
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

/// Luma BT.709 de rango limitado en punto fijo (×256).
#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8
}

/// Croma (Cb, Cr) de la media `(r, g, b)` de un bloque.
#[inline(always)]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let cb = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
    let cr = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (cb.clamp(0, 255) as u8, cr.clamp(0, 255) as u8)
}

/// Camino rápido: bloques 2×2 completos de dos filas, con las posiciones de
/// R, G y B fijas en compilación. Sin comprobaciones de límites por píxel
/// (todo va por `chunks_exact`), lo que deja al compilador vectorizar.
/// Mismo resultado, bit a bit, que el camino general.
#[inline(always)]
fn full_blocks<const RI: usize, const GI: usize, const BI: usize>(
    row0: &[u8],
    row1: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let pairs = row0
        .chunks_exact(8)
        .zip(row1.chunks_exact(8))
        .zip(y0.chunks_exact_mut(2).zip(y1.chunks_exact_mut(2)))
        .zip(u.iter_mut().zip(v.iter_mut()));
    for (((p0, p1), (ya, yb)), (uo, vo)) in pairs {
        let px = |p: &[u8], o: usize| {
            (
                i32::from(p[o + RI]),
                i32::from(p[o + GI]),
                i32::from(p[o + BI]),
            )
        };
        let (a, b, c, d) = (px(p0, 0), px(p0, 4), px(p1, 0), px(p1, 4));
        ya[0] = luma(a.0, a.1, a.2);
        ya[1] = luma(b.0, b.1, b.2);
        yb[0] = luma(c.0, c.1, c.2);
        yb[1] = luma(d.0, d.1, d.2);
        // Media de 4 valores no negativos: `/ 4` = `>> 2`.
        let (cb, cr) = chroma(
            (a.0 + b.0 + c.0 + d.0) >> 2,
            (a.1 + b.1 + c.1 + d.1) >> 2,
            (a.2 + b.2 + c.2 + d.2) >> 2,
        );
        *uo = cb;
        *vo = cr;
    }
}

impl Job<'_> {
    /// Convierte las filas de croma desde `first_crow` que caben en `u`; `y`
    /// empieza en la fila de luma `2·first_crow`.
    fn rows(&self, first_crow: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
        let (w, h) = (self.w, self.h);
        let cw = w.div_ceil(2);
        let full = w / 2;
        for (k, (urow, vrow)) in u.chunks_mut(cw).zip(v.chunks_mut(cw)).enumerate() {
            let r0 = 2 * (first_crow + k);
            let rows: [Option<&[u8]>; 2] = [r0, r0 + 1]
                .map(|r| (r < h).then(|| &self.src[r * self.stride..r * self.stride + w * 4]));
            let mut first_slow = 0;
            if let [Some(row0), Some(row1)] = rows {
                let (top, bottom) = y[2 * k * w..].split_at_mut(w);
                let (y0, y1) = (&mut top[..2 * full], &mut bottom[..2 * full]);
                let (r0b, r1b) = (&row0[..8 * full], &row1[..8 * full]);
                let (uf, vf) = (&mut urow[..full], &mut vrow[..full]);
                match self.offsets {
                    (2, 1, 0) => full_blocks::<2, 1, 0>(r0b, r1b, y0, y1, uf, vf),
                    (0, 1, 2) => full_blocks::<0, 1, 2>(r0b, r1b, y0, y1, uf, vf),
                    (1, 2, 3) => full_blocks::<1, 2, 3>(r0b, r1b, y0, y1, uf, vf),
                    (3, 2, 1) => full_blocks::<3, 2, 1>(r0b, r1b, y0, y1, uf, vf),
                    _ => unreachable!("órdenes de PackedRgb::offsets"),
                }
                first_slow = full;
            }
            // Camino general: columna impar del borde o última fila sola.
            for cc in first_slow..cw {
                let (uo, vo) = (&mut urow[cc], &mut vrow[cc]);
                self.block(&rows, k, cc, y, uo, vo);
            }
        }
    }

    /// Un bloque (recortado en los bordes impares) por el camino general.
    fn block(
        &self,
        rows: &[Option<&[u8]>; 2],
        k: usize,
        cc: usize,
        y: &mut [u8],
        uo: &mut u8,
        vo: &mut u8,
    ) {
        let w = self.w;
        let (ri, gi, bi) = self.offsets;
        let (mut sr, mut sg, mut sb, mut n) = (0, 0, 0, 0);
        for (dr, row) in rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let (c0, c1) = (2 * cc, (2 * cc + 2).min(w));
            let yrow = &mut y[(2 * k + dr) * w + c0..(2 * k + dr) * w + c1];
            for (col, yo) in (c0..c1).zip(yrow.iter_mut()) {
                let p = &row[col * 4..col * 4 + 4];
                let (r, g, b) = (i32::from(p[ri]), i32::from(p[gi]), i32::from(p[bi]));
                *yo = luma(r, g, b);
                sr += r;
                sg += g;
                sb += b;
                n += 1;
            }
        }
        // Media del bloque 2×2 (recortado en bordes impares).
        let (cb, cr) = chroma(sr / n, sg / n, sb / n);
        *uo = cb;
        *vo = cr;
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
