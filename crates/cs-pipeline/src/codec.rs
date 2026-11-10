//! Elección del códec de vídeo (ADR-0012).
//!
//! Por defecto se usa **H.264**, el validado en todas las combinaciones.
//! HEVC y AV1 se piden a propósito (o «automático»). Solo se usa un códec si
//! este equipo lo **codifica** por hardware y el receptor lo **decodifica**;
//! si no, se vuelve a H.264 y se explica por qué.

use cs_core::Codec;

/// Preferencia del usuario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecChoice {
    /// H.264 siempre (por defecto).
    #[default]
    H264,
    /// El más eficiente que soporten los dos lados: AV1, luego HEVC, luego H.264.
    Auto,
    Hevc,
    Av1,
}

impl std::str::FromStr for CodecChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(Self::H264),
            "auto" => Ok(Self::Auto),
            "hevc" | "h265" => Ok(Self::Hevc),
            "av1" => Ok(Self::Av1),
            other => Err(format!(
                "códec desconocido «{other}» (h264, auto, hevc, av1)"
            )),
        }
    }
}

/// Nombre para el usuario.
pub fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

/// Elige el códec. `local`: los que este equipo codifica; `remote`: los que
/// el receptor decodifica. Devuelve el códec y, si no se pudo dar lo pedido,
/// el motivo.
pub fn choose(choice: CodecChoice, local: &[Codec], remote: &[Codec]) -> (Codec, Option<String>) {
    let both = |c: Codec| local.contains(&c) && remote.contains(&c);
    let wanted: &[Codec] = match choice {
        CodecChoice::H264 => return (Codec::H264, None),
        CodecChoice::Auto => &[Codec::Av1, Codec::Hevc],
        CodecChoice::Hevc => &[Codec::Hevc],
        CodecChoice::Av1 => &[Codec::Av1],
    };
    if let Some(&c) = wanted.iter().find(|&&c| both(c)) {
        return (c, None);
    }
    if choice == CodecChoice::Auto {
        return (Codec::H264, None);
    }
    let c = wanted[0];
    let why = if !local.contains(&c) {
        format!(
            "este equipo no codifica {} por hardware; se usa H.264",
            codec_name(c)
        )
    } else {
        format!(
            "el receptor no decodifica {} (en Windows hace falta la extensión de vídeo {} de la Microsoft Store); se usa H.264",
            codec_name(c),
            codec_name(c)
        )
    };
    (Codec::H264, Some(why))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Codec::*;

    #[test]
    fn default_is_h264_even_if_better_is_available() {
        assert_eq!(
            choose(CodecChoice::H264, &[H264, Hevc, Av1], &[H264, Hevc, Av1]),
            (H264, None)
        );
        assert_eq!(CodecChoice::default(), CodecChoice::H264);
    }

    #[test]
    fn auto_picks_the_best_common_codec() {
        assert_eq!(
            choose(CodecChoice::Auto, &[H264, Hevc, Av1], &[H264, Hevc, Av1]).0,
            Av1
        );
        assert_eq!(
            choose(CodecChoice::Auto, &[H264, Hevc], &[H264, Hevc, Av1]).0,
            Hevc
        );
        assert_eq!(
            choose(CodecChoice::Auto, &[H264, Hevc, Av1], &[H264, Av1]).0,
            Av1
        );
        // Receptor Linux (solo H.264): sin aviso, H.264.
        assert_eq!(
            choose(CodecChoice::Auto, &[H264, Hevc], &[H264]),
            (H264, None)
        );
    }

    #[test]
    fn explicit_choice_falls_back_with_a_reason() {
        assert_eq!(
            choose(CodecChoice::Hevc, &[H264, Hevc], &[H264, Hevc]),
            (Hevc, None)
        );
        let (c, why) = choose(CodecChoice::Av1, &[H264, Hevc], &[H264, Hevc, Av1]);
        assert_eq!(c, H264);
        assert!(why.unwrap().contains("no codifica AV1"));
        let (c, why) = choose(CodecChoice::Hevc, &[H264, Hevc], &[H264]);
        assert_eq!(c, H264);
        assert!(why.unwrap().contains("receptor no decodifica HEVC"));
    }

    #[test]
    fn choice_parses() {
        assert_eq!("HEVC".parse::<CodecChoice>(), Ok(CodecChoice::Hevc));
        assert_eq!("h265".parse::<CodecChoice>(), Ok(CodecChoice::Hevc));
        assert_eq!("auto".parse::<CodecChoice>(), Ok(CodecChoice::Auto));
        assert!("vp9".parse::<CodecChoice>().is_err());
    }
}
