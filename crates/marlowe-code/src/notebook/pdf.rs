//! PDFs for `read` (design 2.1 T3): text per page (pdf-extract, behind `catch_unwind` because
//! PDF parsers panic on malformed input) and the `pages` selector. Kept beside notebooks: both are
//! documents `read` renders rather than prints.

/// Pages one `read` returns at most.
pub const MAX_PAGES_PER_READ: usize = 20;

/// Above this many pages, `read` needs `pages`.
pub const PAGES_REQUIRED_OVER: usize = 10;

/// Whether `bytes` start like a PDF.
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// The text of every page, in order.
pub fn page_texts(bytes: &[u8]) -> Result<Vec<String>, String> {
    let owned = bytes.to_vec();
    match std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem_by_pages(&owned)) {
        Ok(Ok(pages)) => Ok(pages),
        Ok(Err(e)) => Err(format!("the PDF could not be read: {e}")),
        Err(_) => Err("the PDF parser failed on this file (it may be damaged or encrypted)".into()),
    }
}

/// Parses `pages` ("3", "1-5", "1,3,7-9") against `total` pages: 1-based, ascending, unique.
pub fn parse_pages(spec: &str, total: usize) -> Result<Vec<usize>, String> {
    let bad = |why: &str| {
        Err(format!(
            "`pages` must look like \"3\", \"1-5\" or \"1,3,7-9\" (1-based, inclusive) — {why}. The \
             document has {total} pages."
        ))
    };
    let mut out: Vec<usize> = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else {
            return bad(&format!("\"{part}\" is not a page or a range"));
        };
        if a == 0 || b == 0 {
            return bad("pages are numbered from 1");
        }
        if b < a {
            return bad(&format!("\"{part}\" runs backwards"));
        }
        if a > total {
            return bad(&format!("page {a} does not exist"));
        }
        out.extend(a..=b.min(total));
        if out.len() > MAX_PAGES_PER_READ * 4 {
            break;
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return bad("no page was named");
    }
    if out.len() > MAX_PAGES_PER_READ {
        return Err(format!(
            "`pages` names {} pages and one `read` returns at most {MAX_PAGES_PER_READ}. Ask for a \
             smaller range, e.g. \"{}-{}\".",
            out.len(),
            out[0],
            out[0] + MAX_PAGES_PER_READ - 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_specs() {
        assert_eq!(parse_pages("2-3", 5).unwrap(), [2, 3]);
        assert_eq!(parse_pages("1, 4,2-3,3", 5).unwrap(), [1, 2, 3, 4]);
        assert_eq!(parse_pages("4-99", 5).unwrap(), [4, 5]);
        for bad in ["0", "3-1", "x", "", "9"] {
            assert!(parse_pages(bad, 5).is_err(), "{bad}");
        }
        assert!(parse_pages("1-21", 50).unwrap_err().contains("at most 20"));
    }
}
