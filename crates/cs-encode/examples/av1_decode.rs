//! Decodifica un flujo AV1 (OBU con tamaño, p. ej. `ffmpeg … -f obu`) con
//! el decodificador por hardware de Windows y comprueba la luma media
//! (ADR-0012). Sirve para validar el receptor AV1 en equipos que no tienen
//! codificador AV1.
//!
//! Uso: `cargo run -p cs-encode --example av1_decode -- <archivo.obu> <luma esperada>`
#![allow(unsafe_code)] // FFI D3D11 en un ejemplo de prueba; bloques acotados.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{anyhow, bail};
    use cs_encode::bitstream::{av1_obu, split_obus};
    use cs_encode::VideoCodec;
    use windows::Win32::Graphics::Direct3D11::*;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .ok_or_else(|| anyhow!("falta el archivo .obu"))?;
    let expected: f32 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(-1.0);
    let data = std::fs::read(path)?;

    // Unidades temporales: empiezan en cada delimitador temporal.
    let obus = split_obus(&data);
    let mut units: Vec<Vec<u8>> = Vec::new();
    for o in &obus {
        if o.obu_type == av1_obu::TEMPORAL_DELIMITER || units.is_empty() {
            units.push(Vec::new());
        }
        units.last_mut().expect("hay una").extend_from_slice(o.data);
    }
    println!(
        "{path}: {} OBU, {} unidades temporales",
        obus.len(),
        units.len()
    );

    let device = cs_decode::create_d3d11_device()?;
    let mut dec = cs_decode::VideoDecoder::new_for(VideoCodec::Av1, &device, Some((1280, 720)))?;
    // SAFETY: dispositivo válido.
    let ctx = unsafe { device.GetImmediateContext() }?;
    let mut decoded = 0usize;
    let mut max_err = 0f32;
    let mut measure = |f: cs_decode::DecodedFrame| -> anyhow::Result<()> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: textura válida del decodificador.
        unsafe { f.texture.GetDesc(&mut d) };
        let sdesc = D3D11_TEXTURE2D_DESC {
            ArraySize: 1,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..d
        };
        let mut staging = None;
        // SAFETY: descriptor derivado del original.
        unsafe { device.CreateTexture2D(&sdesc, None, Some(&mut staging)) }?;
        let staging = staging.ok_or_else(|| anyhow!("staging nula"))?;
        // SAFETY: mismo formato y tamaño.
        unsafe { ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, &f.texture, f.subresource, None) };
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging con CPU_ACCESS_READ.
        unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)) }?;
        let mut sum = 0u64;
        for y in 0..f.height as usize {
            // SAFETY: fila dentro de la región mapeada.
            let row = unsafe {
                std::slice::from_raw_parts(
                    (map.pData as *const u8).add(y * map.RowPitch as usize),
                    f.width as usize,
                )
            };
            sum += row.iter().map(|&b| u64::from(b)).sum::<u64>();
        }
        // SAFETY: empareja el Map.
        unsafe { ctx.Unmap(&staging, 0) };
        let mean = sum as f32 / (f.width as f32 * f.height as f32);
        if expected >= 0.0 {
            max_err = max_err.max((mean - expected).abs());
        }
        decoded += 1;
        Ok(())
    };
    for (i, u) in units.iter().enumerate() {
        if let Some(f) = dec.decode(u, i as i64 * 33_333)? {
            measure(f)?;
        }
        while let Some(f) = dec.next_ready() {
            measure(f)?;
        }
    }
    for f in dec.drain()? {
        measure(f)?;
    }
    println!(
        "AV1: {decoded} frames decodificados por hardware ({}x{}), error máximo de luma {max_err:.1}",
        dec.size().0,
        dec.size().1
    );
    if decoded < units.len().saturating_sub(1) || (expected >= 0.0 && max_err > 4.0) {
        bail!("decodificación AV1 incorrecta");
    }
    println!("✓ correcto");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("av1_decode solo está disponible en Windows");
}
