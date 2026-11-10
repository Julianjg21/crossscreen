//! Sondeo de códecs por hardware y prueba de ida y vuelta (ADR-0012).
//!
//! 1. Lista los códecs que este equipo **codifica** (MFT hardware) y
//!    **decodifica** (MFT con D3D11).
//! 2. Para cada códec con ambos: codifica 60 frames de colores lisos que
//!    cambian, los decodifica y compara la luma media de cada frame con la
//!    esperada (BT.709, rango limitado).
//!
//! Uso: `cargo run -p cs-encode --example codec_probe`
#![allow(unsafe_code)] // FFI D3D11 en un ejemplo de prueba; bloques acotados.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{anyhow, bail};
    use cs_encode::mf::{create_d3d11_device, hw_encoder_codecs, InputFrame, MfH264Encoder};
    use cs_encode::{EncodeControl, EncoderConfig, VideoCodec, VideoEncoder};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    let setup = create_d3d11_device(None)?;
    let enc_codecs = hw_encoder_codecs(&setup.device);
    println!("Adaptador: {}", setup.adapter_name);
    // El decodificador usa su propio dispositivo (como el visor).
    let dec_device = cs_decode::create_d3d11_device()?;
    let dec_codecs = cs_decode::supported_codecs(&dec_device);
    println!("Codifica por hardware: {enc_codecs:?}");
    println!("Decodifica con D3D11:  {dec_codecs:?}");

    let (w, h) = (1280u32, 720u32);
    let colors: Vec<[f32; 4]> = (0..60)
        .map(|i| {
            let t = i as f32 / 59.0;
            [t, 0.5 * (1.0 - t), 0.25 + 0.5 * t, 1.0]
        })
        .collect();
    // Luma BT.709 en rango limitado (16–235).
    let expected_y = |c: [f32; 4]| 16.0 + 219.0 * (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]);

    let mut failures = 0;
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        if !enc_codecs.contains(&codec) || !dec_codecs.contains(&codec) {
            println!("{codec:?}: se omite (falta codificador o decodificador)");
            continue;
        }
        let mut cfg = EncoderConfig::desktop(w, h, 60, 8_000);
        cfg.codec = codec;
        let mut enc = MfH264Encoder::new(&setup.device, cfg)?;
        let mut dec = cs_decode::VideoDecoder::new_for(codec, &dec_device, Some((w, h)))?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: descriptor válido y puntero de salida local.
        unsafe { setup.device.CreateTexture2D(&desc, None, Some(&mut tex)) }?;
        let tex = tex.ok_or_else(|| anyhow!("textura nula"))?;
        let mut rtv = None;
        // SAFETY: textura con BIND_RENDER_TARGET.
        unsafe {
            setup
                .device
                .CreateRenderTargetView(&tex, None, Some(&mut rtv))
        }?;
        let rtv = rtv.ok_or_else(|| anyhow!("RTV nula"))?;

        // Luma media de un frame decodificado (copia a staging NV12). Se mide
        // al momento y se suelta: cada frame retiene una superficie del
        // decodificador y su *pool* es pequeño.
        let measure = |f: &cs_decode::DecodedFrame| -> anyhow::Result<(usize, f32)> {
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
            unsafe { dec_device.CreateTexture2D(&sdesc, None, Some(&mut staging)) }?;
            let staging = staging.ok_or_else(|| anyhow!("staging nula"))?;
            let ctx = unsafe { dec_device.GetImmediateContext() }?;
            // SAFETY: mismo formato y tamaño; subrecurso del array del decodificador.
            unsafe {
                ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, &f.texture, f.subresource, None);
            }
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: staging con CPU_ACCESS_READ.
            unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)) }?;
            let mut sum = 0u64;
            for y in 0..f.height as usize {
                // SAFETY: fila dentro de la región mapeada (RowPitch × alto).
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
            Ok(((f.pts_us / 16_667).clamp(0, 59) as usize, mean))
        };
        let mut max_err = 0f32;
        let mut decoded = 0usize;
        let mut check = |f: cs_decode::DecodedFrame| -> anyhow::Result<()> {
            let (idx, mean) = measure(&f)?;
            max_err = max_err.max((mean - expected_y(colors[idx])).abs());
            decoded += 1;
            Ok(())
        };
        let mut bytes = 0usize;
        let mut keyframes = 0;
        for (n, c) in colors.iter().enumerate() {
            // SAFETY: RTV válida del mismo dispositivo.
            unsafe { setup.context.ClearRenderTargetView(&rtv, c) };
            let frame = InputFrame {
                texture: tex.clone(),
                subresource: 0,
                pts_us: n as u64 * 16_667,
            };
            let units = enc.encode(
                &frame,
                EncodeControl {
                    force_idr: n == 0,
                    bitrate_kbps: None,
                },
            )?;
            for u in units {
                bytes += u.data.len();
                keyframes += usize::from(u.keyframe);
                if let Some(f) = dec.decode(&u.data, u.pts_us as i64)? {
                    check(f)?;
                }
                while let Some(f) = dec.next_ready() {
                    check(f)?;
                }
            }
        }
        for u in enc.flush()? {
            if let Some(f) = dec.decode(&u.data, u.pts_us as i64)? {
                check(f)?;
            }
        }
        for f in dec.drain()? {
            check(f)?;
        }

        println!(
            "{codec:?}: {} frames codificados ({keyframes} clave, {:.1} KiB) → {} decodificados, error máximo de luma {max_err:.1}",
            colors.len(),
            bytes as f64 / 1024.0,
            decoded
        );
        if decoded < colors.len() - 2 || max_err > 6.0 {
            println!("  ✗ FALLO");
            failures += 1;
        } else {
            println!("  ✓ correcto");
        }
    }
    if failures > 0 {
        bail!("{failures} códec(s) fallaron");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("codec_probe solo está disponible en Windows");
}
