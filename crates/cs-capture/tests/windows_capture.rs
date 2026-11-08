//! Pruebas de integración con pantalla real (Windows). Están marcadas
//! `#[ignore]` porque necesitan una sesión de escritorio interactiva:
//! `cargo test -p cs-capture -- --ignored`.
#![cfg(windows)]

use cs_capture::bmp::bgra_content_summary;
use cs_capture::windows_dxgi::{list_outputs, DxgiCapture};
use cs_capture::{CaptureConfig, CaptureSource, CaptureTarget, PixelFormat};
use std::sync::Mutex;
use std::time::Duration;

/// DXGI permite una sola duplicación por salida y proceso: las pruebas que
/// abren la principal se serializan.
static PRIMARY: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    PRIMARY.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
#[ignore = "necesita una sesión de escritorio"]
fn lists_at_least_one_attached_output() {
    let outs = list_outputs().expect("list_outputs");
    assert!(
        outs.iter().any(|o| o.attached_to_desktop),
        "ninguna salida en el escritorio: {outs:?}"
    );
}

#[test]
#[ignore = "necesita una sesión de escritorio"]
fn primary_first_frame_is_real_content() {
    let _g = lock();
    let mut cap = DxgiCapture::new(
        CaptureTarget::Primary,
        CaptureConfig {
            wait_for_gpu_copy: true,
            ..Default::default()
        },
    )
    .expect("abrir captura de la salida principal");
    // Tras crear la duplicación, el primer AcquireNextFrame entrega el
    // escritorio completo aunque nada cambie.
    let frame = (0..20)
        .find_map(|_| {
            cap.next_frame(Duration::from_millis(100))
                .expect("next_frame")
        })
        .expect("sin frame en 2 s");
    assert!(frame.update.full, "el primer frame debe marcarse completo");
    assert_eq!(frame.sequence, 1);
    let info = cap.duplication_info().expect("duplicación activa");
    assert_eq!((frame.width, frame.height), (info.width, info.height));
    if frame.format == PixelFormat::Bgra8 {
        let img = cap.read_to_cpu(&frame.texture).expect("read_to_cpu");
        let (_, colors) = bgra_content_summary(img.width, img.height, img.stride, &img.data);
        assert!(colors > 1, "imagen uniforme: ¿captura negra?");
    }
}

#[test]
#[ignore = "necesita una sesión de escritorio"]
fn unknown_output_is_reported() {
    let err = DxgiCapture::new(
        CaptureTarget::GdiName(r"\\.\DISPLAY999".into()),
        CaptureConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, cs_capture::CaptureError::OutputNotFound(_)));
}

#[test]
#[ignore = "necesita una sesión de escritorio"]
fn pool_reuses_released_textures() {
    let _g = lock();
    let mut cap =
        DxgiCapture::new(CaptureTarget::Primary, CaptureConfig::default()).expect("abrir captura");
    let f1 = (0..20)
        .find_map(|_| cap.next_frame(Duration::from_millis(100)).unwrap())
        .expect("frame");
    let raw1 = f1.texture.texture().clone();
    drop(f1);
    // Recrear la duplicación no es posible sin cambio de modo; basta con
    // comprobar que el siguiente frame (si lo hay) reutiliza la textura.
    if let Some(f2) = cap.next_frame(Duration::from_millis(500)).unwrap() {
        assert_eq!(f2.texture.texture(), &raw1);
    }
}

#[test]
#[ignore = "necesita una sesión de escritorio"]
fn second_duplication_of_same_output_is_rejected() {
    let _g = lock();
    let _first = DxgiCapture::new(CaptureTarget::Primary, CaptureConfig::default())
        .expect("primera captura");
    let err = DxgiCapture::new(CaptureTarget::Primary, CaptureConfig::default()).unwrap_err();
    assert!(
        matches!(err, cs_capture::CaptureError::AlreadyCapturing(_)),
        "{err:?}"
    );
}
