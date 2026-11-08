use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum H264Profile {
    ConstrainedBaseline,
    Main,
    High,
}

/// Capacidad de decodificación (receptor) o codificación (fuente) para un códec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecSupport {
    pub codec: Codec,
    /// Perfil H.264 máximo soportado; `None` para otros códecs.
    pub max_h264_profile: Option<H264Profile>,
    pub hardware: bool,
    /// Máximo de píxeles por segundo (ancho × alto × fps) soportado.
    pub max_pixel_rate: u64,
}

impl CodecSupport {
    /// Elige el mejor códec común entre lo que la fuente puede codificar y el
    /// receptor decodificar. Preferencia: hardware en ambos lados, luego por
    /// orden de `preference`.
    pub fn negotiate<'a>(
        encoders: &'a [CodecSupport],
        decoders: &'a [CodecSupport],
        preference: &[Codec],
    ) -> Option<(&'a CodecSupport, &'a CodecSupport)> {
        let mut best: Option<(&CodecSupport, &CodecSupport, (bool, usize))> = None;
        for e in encoders {
            for d in decoders.iter().filter(|d| d.codec == e.codec) {
                let Some(rank) = preference.iter().position(|c| *c == e.codec) else {
                    continue;
                };
                // Clave: (no-hardware-en-ambos, posición en preferencia); menor es mejor.
                let key = (!(e.hardware && d.hardware), rank);
                if best.as_ref().is_none_or(|b| key < b.2) {
                    best = Some((e, d, key));
                }
            }
        }
        best.map(|(e, d, _)| (e, d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(codec: Codec, hardware: bool) -> CodecSupport {
        CodecSupport {
            codec,
            max_h264_profile: None,
            hardware,
            max_pixel_rate: 0,
        }
    }

    #[test]
    fn prefers_hardware_on_both_sides() {
        let enc = [cs(Codec::H264, true), cs(Codec::Hevc, true)];
        let dec = [cs(Codec::H264, true), cs(Codec::Hevc, false)];
        let (e, _) = CodecSupport::negotiate(&enc, &dec, &[Codec::Hevc, Codec::H264]).unwrap();
        assert_eq!(e.codec, Codec::H264);
    }

    #[test]
    fn respects_preference_when_equal() {
        let enc = [cs(Codec::H264, true), cs(Codec::Hevc, true)];
        let dec = [cs(Codec::H264, true), cs(Codec::Hevc, true)];
        let (e, _) = CodecSupport::negotiate(&enc, &dec, &[Codec::Hevc, Codec::H264]).unwrap();
        assert_eq!(e.codec, Codec::Hevc);
    }

    #[test]
    fn none_when_no_overlap() {
        let enc = [cs(Codec::Av1, true)];
        let dec = [cs(Codec::H264, true)];
        assert!(CodecSupport::negotiate(&enc, &dec, &[Codec::H264, Codec::Av1]).is_none());
    }
}
