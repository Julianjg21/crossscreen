//! Decodifica un archivo Annex-B y mide el tiempo por frame (backend de la
//! plataforma). Uso: `cargo run --release -p cs-decode --example decode_file -- archivo.h264`
//! En Linux necesita el binario de Cisco (ver `scripts/fetch-openh264.sh`).

#[cfg(not(windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cs_decode::annexb::{is_idr, split_access_units};
    use cs_decode::stats::LatencyStats;
    use cs_decode::{create_decoder_context, VideoDecoder};

    let path = std::env::args()
        .nth(1)
        .ok_or("uso: decode_file <archivo.h264>")?;
    let data = std::fs::read(&path)?;
    let aus = split_access_units(&data);
    let ctx = create_decoder_context()?;
    println!("OpenH264: {}", ctx.blob_path().display());
    let mut dec = VideoDecoder::new(&ctx)?;
    let mut stats = LatencyStats::default();
    let (mut frames, mut idr, mut errors) = (0u32, 0u32, 0u32);
    let mut checksum: u64 = 0;
    for (i, au) in aus.iter().enumerate() {
        if is_idr(au) {
            idr += 1;
        }
        match dec.decode(au, i as i64 * 16_666) {
            Ok(Some(f)) => {
                frames += 1;
                stats.record(f.decode_time);
                if frames == 1 {
                    println!(
                        "primer frame: {}x{} strides {:?}",
                        f.width, f.height, f.strides
                    );
                }
                // Suma de una muestra de luma para comprobar que hay contenido.
                checksum = checksum
                    .wrapping_add(f.y.iter().step_by(997).map(|&b| u64::from(b)).sum::<u64>());
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                eprintln!("AU {i}: {e}");
            }
        }
    }
    println!(
        "AUs {} (IDR {idr}) · frames decodificados {frames} · errores {errors} · checksum luma {checksum}",
        aus.len()
    );
    if let Some(s) = stats.summary() {
        println!(
            "decodificación (CPU): p50 {:.2} ms · p95 {:.2} ms · p99 {:.2} ms · máx {:.2} ms (n={})",
            s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms, s.count
        );
    }
    Ok(())
}

#[cfg(windows)]
fn main() {
    eprintln!("en Windows usa crossscreen-viewer --file (backend Media Foundation)");
}
