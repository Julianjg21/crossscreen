//! Ida y vuelta VA-API → OpenH264 (Linux): lo que codifica la GPU debe
//! decodificarse con otro decodificador, parecerse a la imagen original,
//! empezar por un IDR y respetar `force_idr`. Se omite si no hay una GPU
//! que codifique H.264 por VA-API o falta el binario de Cisco.
#![cfg(not(windows))]

use cs_encode::openh264_backend::I420Frame;
use cs_encode::vaapi::VaapiEncoder;
use cs_encode::{EncodeControl, EncoderConfig, VideoEncoder};
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

fn blob() -> Option<std::path::PathBuf> {
    let env = std::env::var_os("CROSSSCREEN_OPENH264").map(std::path::PathBuf::from);
    let home = std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".local/share/crossscreen")
            .join(cs_encode::openh264_backend::OPENH264_BLOB)
    });
    env.into_iter().chain(home).find(|p| p.is_file())
}

/// Escritorio sintético: fondo, texto simulado (rayas finas) y un bloque
/// que se desplaza con `t`.
fn frame(w: u32, h: u32, t: u32) -> I420Frame {
    let (wu, hu) = (w as usize, h as usize);
    let mut y = vec![0u8; wu * hu];
    for row in 0..hu {
        for col in 0..wu {
            let text = (row / 12) % 3 == 0 && (col % 7 < 4) && col > 40;
            let block = col >= (t as usize * 8) % wu
                && col < (t as usize * 8) % wu + 120
                && row > 200
                && row < 330;
            y[row * wu + col] = if block {
                220
            } else if text {
                40
            } else {
                (60 + (col * 100 / wu) + (row * 50 / hu)) as u8
            };
        }
    }
    let c = vec![128u8; (wu / 2) * (hu / 2)];
    I420Frame::new(w, h, y, c.clone(), c, (wu, wu / 2, wu / 2)).unwrap()
}

#[test]
fn vaapi_encode_decodes_with_openh264() {
    let Some(path) = blob() else {
        eprintln!("omitido: falta libopenh264 de Cisco");
        return;
    };
    // 1366 no es múltiplo de 16: comprueba también el recorte.
    let (w, h) = (1366u32, 768u32);
    let cfg = EncoderConfig::desktop(w, h, 60, 8000);
    let mut enc = match VaapiEncoder::new(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("omitido: sin VA-API ({e})");
            return;
        }
    };
    eprintln!("codificador: {}", enc_name(&enc));
    let mut dec = Decoder::with_api_config(
        OpenH264API::from_blob_path(&path).unwrap(),
        DecoderConfig::new(),
    )
    .unwrap();

    let frames = 120u32;
    let mut decoded = 0;
    let mut keyframes = Vec::new();
    let mut times = Vec::new();
    let mut sizes = Vec::new();
    let mut worst_mad = 0.0f64;
    for t in 0..frames {
        let src = frame(w, h, t);
        let force_idr = t == 60;
        let units = enc
            .encode(
                &src,
                EncodeControl {
                    force_idr,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(units.len(), 1);
        let u = &units[0];
        times.push(u.encode_time.as_secs_f64() * 1e3);
        sizes.push(u.data.len());
        if u.keyframe {
            keyframes.push(t);
        }
        let img = dec
            .decode(&u.data)
            .unwrap_or_else(|e| panic!("frame {t} no decodifica: {e}"))
            .unwrap_or_else(|| {
                panic!("frame {t}: el decodificador no devolvió imagen (¿retiene frames?)")
            });
        assert_eq!(img.dimensions(), (w as usize, h as usize));
        decoded += 1;
        // Diferencia media absoluta de luma frente al original.
        let (ys, _, _) = img.strides();
        let got = img.y();
        let mut sum = 0u64;
        for row in 0..h as usize {
            for col in 0..w as usize {
                let a = i32::from(got[row * ys + col]);
                let b = i32::from(src.y[row * w as usize + col]);
                sum += (a - b).unsigned_abs() as u64;
            }
        }
        let mad = sum as f64 / f64::from(w * h);
        worst_mad = worst_mad.max(mad);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    let p_sizes: Vec<_> = sizes
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 0 && *i != 60)
        .map(|(_, s)| *s)
        .collect();
    eprintln!(
        "VA-API {w}x{h}: {decoded}/{frames} decodificados, codificación p50 {:.2} ms p95 {:.2} ms máx {:.2} ms, IDR {} B, P media {} B, QP final {}, peor diferencia media de luma {worst_mad:.2}",
        p(0.5),
        p(0.95),
        times[times.len() - 1],
        sizes[0],
        p_sizes.iter().sum::<usize>() / p_sizes.len().max(1),
        enc.qp(),
    );
    assert_eq!(keyframes, vec![0, 60], "IDR solo al empezar y al pedirlo");
    assert_eq!(decoded, frames);
    assert!(
        worst_mad < 4.0,
        "la imagen decodificada se aleja demasiado: {worst_mad:.2}"
    );
}

fn enc_name(enc: &VaapiEncoder) -> String {
    enc.caps().info.friendly_name.clone()
}
