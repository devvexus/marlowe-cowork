//! Charset resolution and decoding.
//!
//! **This is the module where hand-rolling would have been actively dangerous**, which is why it
//! is the one place in this crate that takes a dependency for correctness rather than for scale.
//!
//! A mis-decoded document does not crash. It produces plausible text — mojibake in the accented
//! characters, silently wrong in CJK — that goes on to be summarised, embedded, ranked and
//! believed. In English-language testing it looks perfect. `encoding_rs` is the WHATWG Encoding
//! Standard implementation that ships in Firefox; it is one crate, it is SIMD-accelerated, and it
//! is the difference between reading the non-English internet and appearing to.
//!
//! # Precedence, and why it is not "whatever the server said"
//!
//! Per the WHATWG rules, in order: **BOM, then the HTTP header, then `<meta charset>`, then a
//! default.** The BOM wins because it is in the bytes themselves. The header outranks the meta
//! tag because a proxy that transcodes a document rewrites the header and cannot rewrite the
//! markup — the meta tag then describes the *original* encoding and is actively wrong.
//!
//! The default for HTML is **windows-1252, not ISO-8859-1**, and not UTF-8. That is a deliberate
//! and slightly surprising choice: the huge population of legacy pages declaring `iso-8859-1`
//! actually contain windows-1252 bytes (curly quotes, em dashes, ellipses in the 0x80..0x9F
//! range), and every browser has decoded them that way for twenty years. Defaulting to UTF-8
//! would turn every one of those characters into a replacement character.

use encoding_rs::Encoding;

use crate::Warning;

/// Where the encoding came from. Kept because "we guessed" and "the document said so" are
/// different facts, and only one of them justifies trusting an odd-looking result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Bom,
    Header,
    Meta,
    Default,
}

#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    pub encoding: &'static str,
    pub source: Source,
}

/// Decode bytes to text, resolving the encoding as described in the module header.
///
/// `html_default` selects windows-1252 (HTML's legacy default) over UTF-8 (correct for JSON, XML
/// and modern plain text). Passing the wrong one is not catastrophic — both are ASCII-compatible
/// — but it decides the fate of the 0x80..0xFF range, which is exactly where the mojibake lives.
pub fn decode(
    bytes: &[u8],
    content_type: Option<&str>,
    html_default: bool,
    warnings: &mut Vec<Warning>,
) -> Decoded {
    // ── 1. BOM. In the bytes, so nothing outranks it. ────────────────────────────────────
    if let Some((enc, len)) = Encoding::for_bom(bytes) {
        return finish(enc, &bytes[len..], Source::Bom, warnings);
    }

    // ── 2. the HTTP header ───────────────────────────────────────────────────────────────
    if let Some(label) = content_type.and_then(charset_param) {
        match Encoding::for_label(label.as_bytes()) {
            Some(enc) => return finish(enc, bytes, Source::Header, warnings),
            None => {
                // Named, unknown. Say so rather than silently falling through — a caller
                // debugging mangled output needs to see the label that was rejected.
                let used = if html_default { encoding_rs::WINDOWS_1252 } else { encoding_rs::UTF_8 };
                warnings.push(Warning::UnknownCharset {
                    declared: label,
                    used: used.name(),
                });
                return finish(used, bytes, Source::Default, warnings);
            }
        }
    }

    // ── 3. <meta charset> ────────────────────────────────────────────────────────────────
    if html_default {
        if let Some(enc) = meta_charset(bytes) {
            return finish(enc, bytes, Source::Meta, warnings);
        }
    }

    // ── 4. the default ───────────────────────────────────────────────────────────────────
    //
    // Valid UTF-8 is checked first even in HTML mode. A modern page that omits every declaration
    // is overwhelmingly UTF-8, and decoding real UTF-8 as windows-1252 mangles it in a way that
    // is *silent* — every byte maps to some character, so nothing signals a problem.
    if std::str::from_utf8(bytes).is_ok() {
        return finish(encoding_rs::UTF_8, bytes, Source::Default, warnings);
    }
    let enc = if html_default { encoding_rs::WINDOWS_1252 } else { encoding_rs::UTF_8 };
    finish(enc, bytes, Source::Default, warnings)
}

fn finish(
    enc: &'static Encoding,
    bytes: &[u8],
    source: Source,
    warnings: &mut Vec<Warning>,
) -> Decoded {
    let (text, actual, had_errors) = enc.decode(bytes);
    if had_errors {
        // Count rather than merely flag: one replacement in a megabyte is a stray byte, and
        // thousands mean the encoding choice was wrong. A boolean cannot tell those apart.
        let replacements = text.chars().filter(|c| *c == '\u{FFFD}').count();
        warnings.push(Warning::LossyDecode { replacements });
    }
    Decoded { text: text.into_owned(), encoding: actual.name(), source }
}

/// Pull `charset=` out of a Content-Type, tolerating quotes and following parameters.
fn charset_param(ct: &str) -> Option<String> {
    let lower = ct.to_ascii_lowercase();
    let at = lower.find("charset")?;
    let rest = lower[at + "charset".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value = rest
        .trim_start_matches('"')
        .split(['"', ';', ' ', '\t'])
        .next()?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Scan the head of a document for `<meta charset>` or `<meta http-equiv content="...charset=">`.
///
/// **Bounded to the first 1024 bytes**, as the WHATWG prescan is. Unbounded, this would be a
/// second full pass over every document to answer a question that is settled in the first line or
/// not at all — and a `charset` appearing three megabytes into a page is not a declaration, it is
/// a coincidence in someone's inline script.
fn meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    const PRESCAN: usize = 1024;
    let head = &bytes[..bytes.len().min(PRESCAN)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();

    let mut from = 0usize;
    while let Some(rel) = text[from..].find("<meta") {
        let start = from + rel;
        let end = text[start..].find('>').map(|e| start + e).unwrap_or(text.len());
        let tag = &text[start..end];

        // <meta charset="utf-8">
        if let Some(v) = attr(tag, "charset") {
            if let Some(enc) = Encoding::for_label(v.as_bytes()) {
                return Some(enc);
            }
        }
        // <meta http-equiv="content-type" content="text/html; charset=utf-8">
        if let Some(content) = attr(tag, "content") {
            if let Some(label) = charset_param(&content) {
                if let Some(enc) = Encoding::for_label(label.as_bytes()) {
                    return Some(enc);
                }
            }
        }
        from = end.max(start + 1);
    }
    None
}

/// Read one attribute value out of an already-lowercased tag. Handles both quote styles and bare
/// values; this is a prescan, not a parser, and it only has to be right about `charset`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(rel) = tag[from..].find(name) {
        let at = from + rel;
        // Must be preceded by whitespace or the tag open, so `content` does not match inside
        // `http-equiv-content`.
        let ok_before = at == 0
            || tag[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        let rest = tag[at + name.len()..].trim_start();
        if ok_before {
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let value = if let Some(r) = rest.strip_prefix('"') {
                    r.split('"').next()
                } else if let Some(r) = rest.strip_prefix('\'') {
                    r.split('\'').next()
                } else {
                    rest.split([' ', '\t', '\n', '/', '>']).next()
                };
                if let Some(v) = value {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        from = at + name.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8], ct: Option<&str>, html: bool) -> (Decoded, Vec<Warning>) {
        let mut w = Vec::new();
        let d = decode(bytes, ct, html, &mut w);
        (d, w)
    }

    #[test]
    fn a_bom_outranks_a_contradicting_header() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice("héllo".as_bytes());
        let (d, _) = dec(&b, Some("text/html; charset=windows-1252"), true);
        assert_eq!(d.source, Source::Bom);
        assert_eq!(d.text, "héllo");
    }

    #[test]
    fn the_header_outranks_the_meta_tag() {
        // The proxy-transcoding case: the meta tag describes the ORIGINAL encoding.
        let html = b"<html><head><meta charset=\"shift_jis\"></head><body>x</body></html>";
        let (d, _) = dec(html, Some("text/html; charset=utf-8"), true);
        assert_eq!(d.source, Source::Header);
        assert_eq!(d.encoding, "UTF-8");
    }

    #[test]
    fn windows_1252_bytes_declared_as_latin1_decode_as_windows_1252() {
        // 0x92 is a curly apostrophe in windows-1252 and undefined in true ISO-8859-1. Every
        // browser reads it the first way; so must we, or a huge population of legacy pages
        // loses its punctuation.
        let (d, _) = dec(b"don\x92t", Some("text/html; charset=iso-8859-1"), true);
        assert_eq!(d.text, "don’t");
    }

    #[test]
    fn an_unknown_declared_charset_is_reported_not_silently_replaced() {
        let (_, w) = dec(b"hello", Some("text/html; charset=bogus-9000"), true);
        assert!(
            w.iter().any(|x| matches!(x, Warning::UnknownCharset { .. })),
            "a rejected charset label must be visible to whoever debugs the output"
        );
    }

    #[test]
    fn undeclared_utf8_is_not_mangled_into_windows_1252() {
        // The silent-failure case: every windows-1252 byte maps to SOME character, so decoding
        // real UTF-8 that way produces mojibake with no error anywhere.
        let (d, w) = dec("naïve café".as_bytes(), None, true);
        assert_eq!(d.text, "naïve café");
        assert!(w.is_empty());
    }

    #[test]
    fn meta_charset_is_read_when_no_header_says_otherwise() {
        let html = b"<html><head><meta charset='utf-8'><title>t</title></head></html>";
        let (d, _) = dec(html, None, true);
        assert_eq!(d.encoding, "UTF-8");
    }

    #[test]
    fn http_equiv_content_type_is_also_read() {
        let html = b"<html><head><meta http-equiv=\"content-type\" content=\"text/html; charset=koi8-r\"></head></html>";
        let (d, _) = dec(html, None, true);
        assert_eq!(d.encoding, "KOI8-R");
        assert_eq!(d.source, Source::Meta);
    }

    #[test]
    fn lossy_decoding_counts_replacements_rather_than_flagging_a_boolean() {
        let (_, w) = dec(&[0xC3, 0x28, 0xC3, 0x28], Some("text/plain; charset=utf-8"), false);
        match w.first() {
            Some(Warning::LossyDecode { replacements }) => assert!(*replacements >= 2),
            other => panic!("expected a counted lossy decode, got {other:?}"),
        }
    }
}
