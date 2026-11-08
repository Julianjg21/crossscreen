//! Genera un flujo H.264 Annex-B sintético con el MFT de software de Media
//! Foundation, para probar el visor sin depender de otros componentes.
//!
//! Uso: `cargo run -p cs-decode --example gen_h264 -- <salida.h264> [ancho alto fps frames]`

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use cs_decode::test_encoder::{synthetic_nv12, TestH264Encoder};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("falta la ruta de salida"))?;
    let num = |i: usize, d: u32| -> anyhow::Result<u32> {
        Ok(args.get(i).map(|s| s.parse()).transpose()?.unwrap_or(d))
    };
    let (w, h, fps, frames) = (num(1, 1920)?, num(2, 1080)?, num(3, 60)?, num(4, 600)?);
    let mut enc = TestH264Encoder::new(w, h, fps, 8_000_000, fps)?;
    let mut data = Vec::new();
    let t0 = std::time::Instant::now();
    for n in 0..frames {
        data.extend(enc.encode(&synthetic_nv12(w, h, n))?);
    }
    data.extend(enc.finish()?);
    std::fs::write(out, &data)?;
    let aus = cs_decode::annexb::split_access_units(&data);
    println!(
        "{out}: {w}x{h}@{fps}, {frames} frames, {} AUs, {} bytes, {:.1} s",
        aus.len(),
        data.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("gen_h264 solo está disponible en Windows");
}
