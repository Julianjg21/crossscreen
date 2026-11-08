# ADR-0001: Licencia `MIT OR Apache-2.0` y no reutilizar código copyleft

**Estado:** Propuesta · **Fecha:** 2026-11-08

## Contexto
Las cinco referencias auditadas tienen licencias copyleft:

| Proyecto | Licencia |
|---|---|
| TwinDesk | GPL-3.0 |
| Sunshine | GPL-3.0 (y moonlight-common-c, también GPL-3.0) |
| Deskreen | AGPL-3.0 |
| Weylus | AGPL-3.0-or-later |
| ScreenExtend | AGPL-3.0 |

Queremos que CrossScreen se pueda integrar en otros productos y que el ecosistema Rust (que usa mayoritariamente `MIT OR Apache-2.0`) pueda reutilizar sus crates.

## Decisión
1. CrossScreen se licencia **`MIT OR Apache-2.0`**.
2. **Se prohíbe copiar, traducir o parafrasear de cerca** código de las cinco referencias. Se permite estudiar su diseño y documentar ideas, siempre citando la fuente en los documentos de diseño.
3. Las dependencias deben tener licencia permisiva (MIT, Apache-2.0, BSD, ISC, Zlib).
   - LGPL solo se admite **enlazada dinámicamente** y documentada como excepción en `deny.toml` (FFmpeg LGPL, `libevdi`).
   - Los módulos del kernel GPL (EVDI) no se distribuyen; los instala la distribución.
4. `cargo-deny` en CI aplica la política. Los PR que tocan áreas cubiertas por las referencias (captura, codificación, driver, emparejamiento) incluyen la declaración "implementación propia, sin código de X".

## Alternativas
- **GPL-3.0:** permitiría reutilizar código de TwinDesk y Sunshine. Se descarta porque impide integrar CrossScreen en productos no GPL y aleja a contribuyentes del ecosistema Rust.
- **AGPL:** mismas razones, y además más restrictiva.

## Consecuencias
- Hay más trabajo de implementación (no hay atajos con código existente).
- Los componentes upstream con licencia MIT (Virtual-Display-Driver, virtual-display-rs, windows-capture) **sí** se pueden usar, tras verificar su archivo LICENSE.
- No se puede incluir x264 ni un FFmpeg compilado como GPL.
