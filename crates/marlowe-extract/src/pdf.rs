//! PDF text extraction.
//!
//! # The coverage decision
//!
//! This is the one format where the library won the argument outright. Hand-rolling PDF properly
//! is xref tables *and* xref streams, object streams, FlateDecode, LZW, encryption, font CMaps,
//! `ToUnicode` maps and text-positioning maths — several thousand lines before it is trustworthy,
//! against a corpus that is relentlessly non-conforming. For an agent whose requirement is *"any
//! document on the internet must be readable"*, coverage beats a smaller dependency tree, and
//! `pdf-extract` carries the font-encoding layer that is the genuinely hard part.
//!
//! # Two failure modes this module exists to make visible
//!
//! **1. The scanned document.** A large fraction of PDFs on the internet are page images with no
//! text layer. They do not error — they extract to `""`. Returned bare, that is indistinguishable
//! from a PDF that genuinely had nothing on it, and a research agent would record "this source
//! says nothing" about a document it simply could not read. So page count is compared against
//! characters recovered, and a document that yields almost nothing per page returns
//! [`Warning::NoTextLayer`] naming the page count. Reading it needs OCR, which this build does
//! not have; saying so is the whole point.
//!
//! **2. The panic.** PDF parsers panic on malformed input, and malformed PDFs are ordinary. In a
//! `rayon` fan-out over three hundred documents an unwinding panic propagates and takes the batch
//! with it — so extraction runs inside [`std::panic::catch_unwind`] and one bad document costs
//! exactly itself.

use crate::{Document, ExtractError, Format, Warning};

/// Below this many characters per page, a PDF is treated as having no usable text layer.
///
/// Deliberately low. Real pages of prose run 1,500–3,000 characters; a title page or a plate
/// section can legitimately be near-empty, so this only fires when essentially nothing came back
/// across the whole document.
const MIN_CHARS_PER_PAGE: usize = 12;

pub fn extract(bytes: &[u8]) -> Result<Document, ExtractError> {
    let mut warnings = Vec::new();

    let pages = page_count(bytes);
    if let Some(detail) = encryption(bytes) {
        // Some encrypted PDFs still extract (empty owner password); try anyway and report either
        // way rather than refusing on the strength of a flag.
        warnings.push(Warning::Encrypted { detail });
    }

    // **The panic guard.** `pdf-extract` unwinds on malformed input, and malformed input is the
    // normal case at corpus scale. Without this, one bad document ends a 300-document batch.
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));

    let raw = match extracted {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            return Err(ExtractError::Backend {
                format: "pdf",
                detail: e.to_string(),
            })
        }
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the PDF parser panicked".to_string());
            return Err(ExtractError::Backend {
                format: "pdf",
                detail: format!("{detail} (recovered; the rest of the batch is unaffected)"),
            });
        }
    };

    let text = crate::normalize(&raw, &mut warnings);

    // ── the scanned-document check ───────────────────────────────────────────────────────
    let recovered = text.trim().len();
    let effective_pages = pages.max(1);
    if recovered < effective_pages * MIN_CHARS_PER_PAGE {
        warnings.push(Warning::NoTextLayer { pages: effective_pages });
    }

    Ok(Document {
        format: Format::Pdf,
        title: title(bytes),
        text,
        links: Vec::new(),
        headings: Vec::new(),
        lang: None,
        description: None,
        bytes_in: bytes.len(),
        encoding: "UTF-8",
        warnings,
    })
}

/// Count pages.
///
/// `lopdf` is already in the tree beneath `pdf-extract`, so naming it costs **zero** additional
/// crates and buys a real page count instead of a guess. When the document cannot be loaded at
/// all this falls back to counting `/Type /Page` markers in the raw bytes, which is wrong for
/// object-stream PDFs but is only ever used to scale a warning threshold.
fn page_count(bytes: &[u8]) -> usize {
    if let Ok(doc) = std::panic::catch_unwind(|| lopdf::Document::load_mem(bytes)) {
        if let Ok(doc) = doc {
            let n = doc.get_pages().len();
            if n > 0 {
                return n;
            }
        }
    }
    let mut n = 0usize;
    let needle = b"/Type";
    let mut i = 0usize;
    while let Some(rel) = memchr::memchr(b'/', &bytes[i..]) {
        let at = i + rel;
        if bytes[at..].starts_with(needle) {
            let tail = &bytes[at + needle.len()..(at + needle.len() + 16).min(bytes.len())];
            let s = String::from_utf8_lossy(tail);
            let s = s.trim_start();
            if s.starts_with("/Page") && !s.starts_with("/Pages") {
                n += 1;
            }
        }
        i = at + 1;
        if i >= bytes.len() {
            break;
        }
    }
    n
}

/// Detect an encryption dictionary. Reported, not treated as fatal — many PDFs are "encrypted"
/// with an empty user password and extract perfectly well.
fn encryption(bytes: &[u8]) -> Option<String> {
    let window = &bytes[..bytes.len().min(4096)];
    let tail_start = bytes.len().saturating_sub(4096);
    for w in [window, &bytes[tail_start..]] {
        if contains(w, b"/Encrypt") {
            return Some("the document declares an /Encrypt dictionary".to_string());
        }
    }
    None
}

/// Document title from the info dictionary, when it is plainly present.
fn title(bytes: &[u8]) -> Option<String> {
    let doc = std::panic::catch_unwind(|| lopdf::Document::load_mem(bytes)).ok()?.ok()?;
    let info = doc.trailer.get(b"Info").ok()?;
    let dict = match info {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok()?.as_dict().ok()?,
        lopdf::Object::Dictionary(d) => d,
        _ => return None,
    };
    let raw = dict.get(b"Title").ok()?.as_str().ok()?;
    let s = decode_pdf_text(raw);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// PDF text strings are either PDFDocEncoding or UTF-16BE with a BOM.
fn decode_pdf_text(raw: &[u8]) -> String {
    if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    // PDFDocEncoding agrees with Latin-1 across the range that appears in titles.
    raw.iter().map(|b| *b as char).collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memchr_iter(needle[0], hay).any(|i| hay[i..].starts_with(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid, minimal one-page PDF with no text operators — i.e. the shape of a
    /// scanned page once the image is stripped.
    fn textless_pdf() -> Vec<u8> {
        let body = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
trailer<</Root 1 0 R>>\n%%EOF\n";
        body.to_vec()
    }

    #[test]
    fn a_pdf_with_no_text_layer_is_reported_not_returned_as_an_empty_success() {
        // THE case this module exists for: an empty string here is indistinguishable from a
        // document that had nothing to say, and a research agent would record the wrong fact.
        let d = extract(&textless_pdf());
        match d {
            Ok(doc) => {
                assert!(!doc.has_text());
                assert!(
                    doc.warnings.iter().any(|w| matches!(w, Warning::NoTextLayer { .. })),
                    "a scanned document must announce that it needs OCR, got {:?}",
                    doc.warnings
                );
            }
            // Some builds refuse this minimal file outright; an error is also an honest answer.
            Err(_) => {}
        }
    }

    #[test]
    fn garbage_claiming_to_be_a_pdf_errors_rather_than_taking_the_batch_down() {
        let mut junk = b"%PDF-1.7\n".to_vec();
        junk.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF].repeat(500));
        // The contract: this returns, one way or the other. It must never unwind into the caller,
        // because the caller is a rayon fan-out over hundreds of documents.
        let _ = extract(&junk);
    }

    #[test]
    fn utf16_titles_decode() {
        let raw = [0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69];
        assert_eq!(decode_pdf_text(&raw), "Hi");
    }

    #[test]
    fn an_encrypt_dictionary_is_noticed() {
        let mut b = b"%PDF-1.6\n".to_vec();
        b.extend_from_slice(b"trailer<</Encrypt 9 0 R>>");
        assert!(encryption(&b).is_some());
    }
}
