//! Codifica imágenes sintéticas con el MFT de software y las decodifica con
//! el MFT hardware: comprueba nº de frames, tamaño, pts y que la textura de
//! salida es NV12 en GPU.
#![cfg(windows)]

use cs_decode::annexb::{is_idr, nal_types, split_access_units};
use cs_decode::test_encoder::{synthetic_nv12, TestH264Encoder};
use cs_decode::{create_d3d11_device, VideoDecoder};

fn encode(w: u32, h: u32, frames: u32) -> Vec<u8> {
    let mut enc = TestH264Encoder::new(w, h, 30, 4_000_000, 30).expect("encoder");
    let mut data = Vec::new();
    for n in 0..frames {
        data.extend(enc.encode(&synthetic_nv12(w, h, n)).expect("encode"));
    }
    data.extend(enc.finish().expect("finish"));
    data
}

#[test]
fn encode_split_decode_roundtrip() {
    let (w, h, frames) = (640u32, 360u32, 45u32);
    let data = encode(w, h, frames);
    let aus = split_access_units(&data);
    assert_eq!(
        aus.len(),
        frames as usize,
        "AUs: {:?}",
        aus.iter().map(|a| nal_types(a)).collect::<Vec<_>>()
    );
    assert!(is_idr(aus[0]));
    assert!(
        nal_types(aus[0]).contains(&7),
        "SPS en línea en la primera AU"
    );

    let device = create_d3d11_device().expect("device");
    let mut dec = match VideoDecoder::new(&device) {
        Ok(d) => d,
        // Sin decodificación H.264 por hardware (p. ej. runners de CI sin GPU):
        // se omite de forma explícita en vez de fallar. En equipos con GPU sí se ejecuta.
        Err(cs_decode::DecodeError::Unsupported(msg)) => {
            eprintln!(
                "OMITIDO: este equipo no tiene decodificación H.264 por hardware (DXVA): {msg}"
            );
            return;
        }
        Err(e) => panic!("decoder: {e:?}"),
    };
    // Se guarda solo un resumen de cada frame y se suelta enseguida para
    // devolver la superficie al pool del MFT.
    let mut all = Vec::new();
    let summary = |f: cs_decode::DecodedFrame| (f.width, f.height, f.pts_us, desc_ok(&f));
    for (i, au) in aus.iter().enumerate() {
        if let Some(f) = dec.decode(au, i as i64 * 33_333).expect("decode") {
            all.push(summary(f));
        }
        while let Some(f) = dec.next_ready() {
            all.push(summary(f));
        }
    }
    all.extend(dec.drain().expect("drain").into_iter().map(summary));
    assert_eq!(all.len(), frames as usize);
    for (i, &(fw, fh, pts, ok)) in all.iter().enumerate() {
        assert_eq!((fw, fh), (w, h));
        assert_eq!(pts, i as i64 * 33_333);
        assert!(ok, "textura NV12");
    }
    assert!(dec.stream_changes() >= 1);
}

#[allow(unsafe_code)]
fn desc_ok(f: &cs_decode::DecodedFrame) -> bool {
    use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
    let mut d = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: textura válida, puntero a una estructura local.
    unsafe { f.texture.GetDesc(&mut d) };
    d.Format == DXGI_FORMAT_NV12
        && d.Width >= f.width
        && d.Height >= f.height
        && f.subresource < d.ArraySize.max(1)
}
