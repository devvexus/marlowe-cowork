//! Format detection. **Bytes decide; the declared type is only a hint.**
//!
//! A server that says `text/html` while serving a PDF, or `application/octet-stream` while
//! serving perfectly ordinary HTML, is common enough on the open internet that trusting the
//! header would make the extractor wrong in exactly the cases a research agent cares about —
//! institutional repositories, preprint mirrors and government portals are the worst offenders.
//!
//! So the order is: **magic bytes, then declared type, then URL extension, then shape.** Each
//! step only runs when the one before it was silent, and the first three are cheap constant-time
//! checks on a prefix.

use crate::Format;

/// How many leading bytes the shape heuristics look at. Enough to clear any BOM, XML
/// declaration, licence comment or doctype without walking a 60 MB document to decide what it is.
const PEEK: usize = 4096;

/// Detect the format.
pub fn detect(bytes: &[u8], content_type: Option<&str>, url: Option<&str>) -> Format {
    // ── 1. magic bytes ───────────────────────────────────────────────────────────────────
    // Unambiguous and unspoofable by a header. This is why a mislabelled PDF still reads.
    if bytes.starts_with(b"%PDF-") {
        return Format::Pdf;
    }
    if is_zip(bytes) {
        return zip_flavour(bytes);
    }
    // A PDF preceded by junk still starts a PDF; some servers emit a stray newline or BOM.
    if let Some(head) = bytes.get(..8) {
        if head.windows(5).any(|w| w == b"%PDF-") {
            return Format::Pdf;
        }
    }

    let peek = &bytes[..bytes.len().min(PEEK)];
    let text = String::from_utf8_lossy(strip_bom(peek));
    let lowered = text.trim_start().to_ascii_lowercase();

    // ── 2. markup shape, which outranks a declared type ──────────────────────────────────
    // `<!doctype html>` is not something a non-HTML document says by accident.
    if lowered.starts_with("<!doctype html") || lowered.starts_with("<html") {
        return Format::Html;
    }

    // ── 3. the declared type ─────────────────────────────────────────────────────────────
    if let Some(ct) = content_type {
        if let Some(f) = from_mime(ct) {
            // An XML *declaration* on something declared HTML means XHTML, which the HTML
            // extractor reads correctly and the XML extractor would strip to nothing useful.
            if f == Format::Xml && lowered.contains("<html") {
                return Format::Html;
            }
            return f;
        }
    }

    // ── 4. the URL extension ─────────────────────────────────────────────────────────────
    if let Some(f) = url.and_then(from_extension) {
        return f;
    }

    // ── 5. shape, last ───────────────────────────────────────────────────────────────────
    if lowered.starts_with("<?xml") {
        return if lowered.contains("<html") { Format::Html } else { Format::Xml };
    }
    // A bare `<html`-less fragment is still HTML if it opens with a tag we recognise. Common for
    // partial responses and embedded documents.
    if lowered.starts_with("<head")
        || lowered.starts_with("<body")
        || lowered.starts_with("<!--")
        || starts_with_common_tag(&lowered)
    {
        return Format::Html;
    }
    if looks_like_json(&lowered) {
        return Format::Json;
    }
    if is_probably_binary(peek) {
        return Format::Unsupported;
    }
    // Text of some kind. Markdown and CSV are *shapes* of plain text; misreading one as the other
    // costs nothing, because all three extractors preserve the characters.
    if looks_like_csv(&text) {
        return Format::Csv;
    }
    if looks_like_markdown(&text) {
        return Format::Markdown;
    }
    Format::PlainText
}

fn is_zip(bytes: &[u8]) -> bool {
    // Local file header, or an empty/spanned archive.
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") || bytes.starts_with(b"PK\x07\x08")
}

/// Which OOXML/epub flavour a ZIP is.
///
/// **Read from the archive's own part names, not from the extension.** A `.zip` that contains
/// `word/document.xml` is a `.docx` whatever it is called, and a research agent downloading from
/// a repository sees that constantly.
fn zip_flavour(bytes: &[u8]) -> Format {
    // epub states its own type, uncompressed, at a fixed offset — the one self-describing case.
    if bytes.len() > 58 && &bytes[30..38] == b"mimetype" {
        if bytes[38..].starts_with(b"application/epub+zip") {
            return Format::Epub;
        }
    }
    // Otherwise look for the signature part name. Scanning a bounded window of the archive is
    // enough: OOXML writers put the content-types part and the main document early, and the
    // central directory repeats every name at the end.
    let head = &bytes[..bytes.len().min(64 * 1024)];
    let tail_start = bytes.len().saturating_sub(64 * 1024);
    let tail = &bytes[tail_start..];
    for window in [head, tail] {
        if contains(window, b"word/document.xml") {
            return Format::Docx;
        }
        if contains(window, b"xl/workbook.xml") {
            return Format::Xlsx;
        }
        if contains(window, b"ppt/presentation.xml") {
            return Format::Pptx;
        }
        if contains(window, b"META-INF/container.xml") {
            return Format::Epub;
        }
    }
    Format::Unsupported
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    // `memchr` on the first byte, then verify — the standard trick, and it keeps this linear
    // without pulling a substring-search crate in for four call sites.
    memchr::memchr_iter(needle[0], haystack)
        .any(|i| haystack[i..].starts_with(needle))
}

pub(crate) fn strip_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    }
}

fn from_mime(ct: &str) -> Option<Format> {
    // `text/html; charset=utf-8` — the parameters belong to the charset layer.
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match mime.as_str() {
        "text/html" | "application/xhtml+xml" => Format::Html,
        "application/pdf" | "application/x-pdf" => Format::Pdf,
        "application/json" | "text/json" | "application/ld+json" => Format::Json,
        "text/csv" | "text/tab-separated-values" => Format::Csv,
        "text/markdown" | "text/x-markdown" => Format::Markdown,
        "text/plain" => Format::PlainText,
        "text/xml" | "application/xml" | "application/rss+xml" | "application/atom+xml" => {
            Format::Xml
        }
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Format::Docx,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Format::Xlsx,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Format::Pptx
        }
        "application/epub+zip" => Format::Epub,
        _ => return None,
    })
}

fn from_extension(url: &str) -> Option<Format> {
    // Query and fragment are not part of the path, and `?format=pdf` must not read as one.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" | "xhtml" => Format::Html,
        "pdf" => Format::Pdf,
        "json" | "jsonl" | "ndjson" => Format::Json,
        "csv" | "tsv" => Format::Csv,
        "md" | "markdown" => Format::Markdown,
        "txt" | "text" | "log" => Format::PlainText,
        "xml" | "rss" | "atom" => Format::Xml,
        "docx" => Format::Docx,
        "xlsx" => Format::Xlsx,
        "pptx" => Format::Pptx,
        "epub" => Format::Epub,
        _ => return None,
    })
}

fn starts_with_common_tag(lowered: &str) -> bool {
    const TAGS: [&str; 8] = [
        "<div", "<p>", "<p ", "<span", "<table", "<article", "<section", "<meta",
    ];
    TAGS.iter().any(|t| lowered.starts_with(t))
}

fn looks_like_json(lowered: &str) -> bool {
    let t = lowered.trim_start();
    // A lone `{` opens a great many things; requiring a quoted key or an obvious empty container
    // keeps CSS and code from reading as JSON.
    (t.starts_with('{') && (t.contains("\":") || t.starts_with("{}")))
        || (t.starts_with('[') && (t.contains('{') || t.contains('"') || t.starts_with("[]")))
}

/// Consistent delimiter counts across the first several lines. Deliberately conservative: prose
/// containing commas must not read as CSV, so a single column never qualifies.
fn looks_like_csv(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(5).collect();
    if lines.len() < 2 {
        return false;
    }
    for delim in [',', '\t', ';'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(delim).count()).collect();
        if counts[0] >= 1 && counts.iter().all(|c| *c == counts[0]) {
            return true;
        }
    }
    false
}

fn looks_like_markdown(text: &str) -> bool {
    let mut score = 0usize;
    for line in text.lines().take(80) {
        let t = line.trim_start();
        if t.starts_with("# ") || t.starts_with("## ") || t.starts_with("### ") {
            score += 2;
        }
        if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("> ") {
            score += 1;
        }
        if t.starts_with("```") {
            score += 2;
        }
        if t.contains("](") {
            score += 1;
        }
    }
    score >= 3
}

/// NUL bytes and a high proportion of C0 control characters. The standard sniff, and it is why an
/// unrecognised binary returns `Unsupported` instead of a page of replacement characters.
fn is_probably_binary(peek: &[u8]) -> bool {
    if peek.contains(&0) {
        return true;
    }
    let ctrl = peek
        .iter()
        .filter(|b| **b < 0x09 || (**b > 0x0D && **b < 0x20))
        .count();
    ctrl * 100 > peek.len().max(1) * 5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_bytes_beat_a_lying_content_type() {
        // The case that matters: repositories serve PDFs as text/html constantly.
        assert_eq!(detect(b"%PDF-1.7\nstuff", Some("text/html"), None), Format::Pdf);
    }

    #[test]
    fn a_doctype_beats_a_lying_content_type_in_the_other_direction() {
        assert_eq!(
            detect(b"<!DOCTYPE html><html><body>hi</body></html>", Some("application/octet-stream"), None),
            Format::Html
        );
    }

    #[test]
    fn xhtml_declared_as_xml_reads_as_html() {
        let b = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><body>x</body></html>"#;
        assert_eq!(detect(b, Some("application/xml"), None), Format::Html);
    }

    #[test]
    fn a_zip_is_classified_by_its_parts_not_its_extension() {
        let mut z = b"PK\x03\x04".to_vec();
        z.extend_from_slice(&[0u8; 26]);
        z.extend_from_slice(b"word/document.xml");
        z.resize(400, b' ');
        assert_eq!(detect(&z, None, Some("https://x/report.zip")), Format::Docx);
    }

    #[test]
    fn an_epub_states_its_own_type() {
        let mut z = b"PK\x03\x04".to_vec();
        z.resize(30, 0);
        z.extend_from_slice(b"mimetypeapplication/epub+zip");
        z.resize(400, b' ');
        assert_eq!(detect(&z, None, None), Format::Epub);
    }

    #[test]
    fn a_query_string_is_not_an_extension() {
        assert_ne!(detect(b"plain words here", None, Some("https://x/a?format=.pdf")), Format::Pdf);
    }

    #[test]
    fn prose_with_commas_is_not_csv() {
        let prose = "Hello there, friend.\nThis is a sentence, with commas, in it.\nAnd more.";
        assert_eq!(detect(prose.as_bytes(), None, None), Format::PlainText);
    }

    #[test]
    fn consistent_columns_are_csv() {
        assert_eq!(detect(b"a,b,c\n1,2,3\n4,5,6", None, None), Format::Csv);
    }

    #[test]
    fn nul_bytes_mean_binary_not_a_page_of_replacement_characters() {
        assert_eq!(detect(&[0xFF, 0x00, 0x01, 0x02, 0x00], None, None), Format::Unsupported);
    }
}
