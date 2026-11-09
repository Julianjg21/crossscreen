//! Ida y vuelta OpenH264 (Linux): lo que produce el codificador del emisor
//! debe poder decodificarse, empezar por un IDR y respetar `force_idr`.
//! Se omite si no está el binario de Cisco (`scripts/fetch-openh264.sh` o
//! `CROSSSCREEN_OPENH264`).
#![cfg(not(windows))]

use cs_encode::openh264_backend::{I420Frame, OpenH264Encoder};
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

fn frame(w: u32, h: u32, t: u32) -> I420Frame {
    let (wu, hu) = (w as usize, h as usize);
    let y = (0..wu * hu)
        .map(|i| (16 + ((i % wu) as u32 + t * 4) % 200) as u8)
        .collect();
    let c = vec![128u8; (wu / 2) * (hu / 2)];
    I420Frame::new(w, h, y, c.clone(), c, (wu, wu / 2, wu / 2)).unwrap()
}

#[test]
fn encode_decode_roundtrip() {
    let Some(path) = blob() else {
        eprintln!("omitido: falta libopenh264 de Cisco");
        return;
    };
    let (w, h) = (640u32, 360u32);
    let cfg = EncoderConfig::desktop(w, h, 60, 4000);
    let mut enc = OpenH264Encoder::from_blob_path(&path, &cfg).unwrap();
    let mut dec = Decoder::with_api_config(
        OpenH264API::from_blob_path(&path).unwrap(),
        DecoderConfig::new(),
    )
    .unwrap();

    let mut decoded = 0;
    let mut keyframes = Vec::new();
    for t in 0..30u32 {
        let force_idr = t == 0 || t == 20;
        let units = enc
            .encode(
                &frame(w, h, t),
                EncodeControl {
                    force_idr,
                    ..Default::default()
                },
            )
            .unwrap();
        for u in units {
            if u.keyframe {
                keyframes.push(t);
            }
            if let Some(img) = dec.decode(&u.data).unwrap() {
                assert_eq!(img.dimensions(), (w as usize, h as usize));
                decoded += 1;
            }
        }
    }
    assert_eq!(keyframes.first(), Some(&0), "el primer frame debe ser IDR");
    assert!(
        keyframes.contains(&20),
        "force_idr no generó un IDR: {keyframes:?}"
    );
    assert!(
        decoded >= 28,
        "solo se decodificaron {decoded} de 30 frames"
    );
}
