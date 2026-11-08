//! Forma canónica del texto y conversiones UTF-8 ⇄ UTF-16.
//!
//! **Forma canónica (la que viaja por la red y se compara):**
//! - saltos de línea normalizados a `\n` (`\r\n` y `\r` sueltos → `\n`);
//! - truncado en el primer NUL, igual que hace Windows al leer
//!   `CF_UNICODETEXT` (si no, un texto con NUL escrito y releído no
//!   coincidiría consigo mismo y provocaría un eco).

/// Normaliza saltos de línea a `\n` y corta en el primer NUL.
pub fn canonicalize(s: &str) -> String {
    let s = match s.find('\0') {
        Some(i) => &s[..i],
        None => s,
    };
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Texto canónico (`\n`) → convención de Windows (`\r\n`).
pub fn to_windows_newlines(canonical: &str) -> String {
    let extra = canonical.matches('\n').count();
    let mut out = String::with_capacity(canonical.len() + extra);
    for c in canonical.chars() {
        if c == '\n' {
            out.push('\r');
        }
        out.push(c);
    }
    out
}

/// UTF-8 → UTF-16 terminado en NUL (para `CF_UNICODETEXT`). Los caracteres
/// fuera del plano básico (emojis) se codifican como pares subrogados.
pub fn to_utf16_nul(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// UTF-16 (posiblemente con NUL final o basura tras él) → UTF-8. Se detiene en
/// el primer NUL. Las subrogadas sueltas (texto mal formado de otro programa)
/// se sustituyen por U+FFFD en lugar de fallar.
pub fn from_utf16_nul(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_newlines() {
        assert_eq!(canonicalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(canonicalize("\r\n\r\n"), "\n\n");
        assert_eq!(canonicalize("\r"), "\n");
        assert_eq!(canonicalize("sin saltos"), "sin saltos");
        // Idempotente.
        let once = canonicalize("x\r\ny\r");
        assert_eq!(canonicalize(&once), once);
    }

    #[test]
    fn canonicalize_truncates_at_nul() {
        assert_eq!(canonicalize("antes\0después"), "antes");
        assert_eq!(canonicalize("\0"), "");
    }

    #[test]
    fn windows_newlines_roundtrip() {
        let c = "l1\nl2\n\nfin";
        let w = to_windows_newlines(c);
        assert_eq!(w, "l1\r\nl2\r\n\r\nfin");
        assert_eq!(canonicalize(&w), c);
    }

    #[test]
    fn utf16_roundtrip_with_emoji_and_combining() {
        for s in [
            "👍🏽 familia 👨‍👩‍👧‍👦 bandera 🇨🇴",
            "acentos áéíóú ñ, combinado e\u{301}",
            "漢字かな 한국어 עברית العربية",
            "𝄞 (fuera del BMP)",
            "",
        ] {
            let w = to_utf16_nul(s);
            assert_eq!(*w.last().unwrap(), 0, "termina en NUL");
            assert_eq!(from_utf16_nul(&w), s);
        }
        // 👍 ocupa un par subrogado (2 unidades) + NUL.
        assert_eq!(to_utf16_nul("👍").len(), 3);
    }

    #[test]
    fn from_utf16_stops_at_first_nul_and_ignores_trailing_garbage() {
        let mut w = to_utf16_nul("hola");
        w.extend([0x41, 0x42, 0]); // basura tras el NUL (búferes de GlobalAlloc)
        assert_eq!(from_utf16_nul(&w), "hola");
        // Sin NUL: usa todo el búfer.
        assert_eq!(from_utf16_nul(&[0x61, 0x62]), "ab");
    }

    #[test]
    fn lone_surrogates_are_replaced_not_fatal() {
        // Subrogada alta sin pareja, seguida de texto.
        let w = [0xD83D, 0x0041, 0];
        assert_eq!(from_utf16_nul(&w), "\u{FFFD}A");
    }
}
