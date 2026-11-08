//! Escritura de BMP de 32 bpp para depurar (volcar UN frame a disco).
//!
//! Solo para diagnóstico: el camino caliente nunca lee la imagen a CPU.

/// Codifica una imagen BGRA8 (`stride` bytes por fila) como BMP de 32 bpp
/// de arriba abajo. El alfa se fuerza a 255 (el escritorio no lo usa y
/// algunos visores lo interpretan).
///
/// Devuelve `None` si las dimensiones no cuadran con el buffer.
pub fn encode_bgra_bmp(width: u32, height: u32, stride: usize, bgra: &[u8]) -> Option<Vec<u8>> {
    let row = width as usize * 4;
    if width == 0 || height == 0 || stride < row {
        return None;
    }
    if bgra.len() < stride * (height as usize - 1) + row {
        return None;
    }
    let pixels = row * height as usize;
    let file_size = 14 + 40 + pixels;
    let mut out = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER (alto negativo = filas de arriba abajo)
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(-(height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(pixels as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 ppp
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in 0..height as usize {
        let src = &bgra[y * stride..y * stride + row];
        for px in src.chunks_exact(4) {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    Some(out)
}

/// Resumen del contenido de una imagen BGRA8 para comprobar que no es negra
/// ni uniforme: (media de luminancia 0..255, nº de colores distintos en una
/// muestra de hasta 4096 píxeles).
pub fn bgra_content_summary(width: u32, height: u32, stride: usize, bgra: &[u8]) -> (f64, usize) {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return (0.0, 0);
    }
    let mut sum = 0f64;
    let mut n = 0usize;
    let mut colors = std::collections::HashSet::new();
    let step_y = (h / 64).max(1);
    let step_x = (w / 64).max(1);
    for y in (0..h).step_by(step_y) {
        for x in (0..w).step_by(step_x) {
            let i = y * stride + x * 4;
            if i + 3 >= bgra.len() {
                continue;
            }
            let (b, g, r) = (bgra[i] as f64, bgra[i + 1] as f64, bgra[i + 2] as f64);
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1;
            colors.insert([bgra[i], bgra[i + 1], bgra[i + 2]]);
        }
    }
    (if n == 0 { 0.0 } else { sum / n as f64 }, colors.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_header_and_padding_removed() {
        // 2×2 con stride 12 (4 bytes de relleno por fila).
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(&[1, 2, 3, 0]);
        data[12..16].copy_from_slice(&[4, 5, 6, 0]);
        let bmp = encode_bgra_bmp(2, 2, 12, &data).unwrap();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 54 + 16);
        assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()), 70);
        assert_eq!(i32::from_le_bytes(bmp[22..26].try_into().unwrap()), -2);
        assert_eq!(&bmp[54..58], &[1, 2, 3, 255]);
        assert_eq!(&bmp[62..66], &[4, 5, 6, 255]);
    }

    #[test]
    fn bmp_rejects_bad_dimensions() {
        assert!(encode_bgra_bmp(0, 1, 4, &[0; 4]).is_none());
        assert!(encode_bgra_bmp(2, 2, 4, &[0; 16]).is_none()); // stride < fila
        assert!(encode_bgra_bmp(2, 2, 8, &[0; 15]).is_none()); // corto
    }

    #[test]
    fn content_summary_detects_black() {
        let black = vec![0u8; 16 * 16 * 4];
        assert_eq!(bgra_content_summary(16, 16, 64, &black), (0.0, 1));
        let mut varied = black.clone();
        varied[0..4].copy_from_slice(&[255, 255, 255, 255]);
        let (_, colors) = bgra_content_summary(16, 16, 64, &varied);
        assert_eq!(colors, 2);
    }
}
