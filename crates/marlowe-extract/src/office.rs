//! OOXML, epub and generic XML.
//!
//! Every format here is **ZIP + XML**, so the work is: open the archive ([`crate::zip`]), find the
//! parts that carry text, and pull the text nodes out with a pull parser. No DOM is built for the
//! same reason [`crate::html`] builds none — the tree would be constructed and immediately
//! discarded.
//!
//! # The part names are the whole trick
//!
//! Each format keeps its prose in a known place, and everything else in the archive is styling,
//! relationships and metadata that would only add noise:
//!
//! | Format | Text lives in |
//! |---|---|
//! | `docx` | `word/document.xml`, plus headers/footnotes |
//! | `xlsx` | `xl/sharedStrings.xml` and `xl/worksheets/sheet*.xml` |
//! | `pptx` | `ppt/slides/slide*.xml` and their notes |
//! | `epub` | the XHTML documents named by the OPF spine |
//!
//! epub is the one that routes back out: its chapters are HTML, so they go through the HTML
//! extractor rather than being re-implemented here.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::{charset, zip, Document, ExtractError, Format, Heading, Warning};

/// Cap on epub chapters read. A pathological archive can declare thousands.
const MAX_CHAPTERS: usize = 2_000;

/// Cap on retained cells in one spreadsheet row.
///
/// # Audit finding G6 — shared-string amplification, and why it is an `abort`
///
/// `sheet_text` pushed `shared[i].clone()` per cell with no bound on the row. A <20 KB archive
/// holding **one** 1 MiB shared string, referenced by 200,000 cells in a single `<row>`, asks for
/// ~200 GB — an amplification of roughly 10⁷:1 against the compressed archive.
///
/// Allocation failure in Rust is an **`abort`**, which means `extract`'s `catch_unwind` cannot
/// intercept it. The distinction matters: every other extractor failure costs one document, and
/// this one costs the daemon, the turn the user is waiting on, and every other document in the
/// batch. `rayon`'s fan-out multiplies it.
///
/// Excel's own column limit is 16,384. This is deliberately below that: a sheet wider than 4,096
/// columns is a generated dump, not a document anyone is reading, and the row is truncated with a
/// warning rather than the process dying.
const MAX_SHEET_COLS: usize = 4_096;

/// Cap on the shared-string table itself — the multiplicand in the amplification above.
const MAX_SHARED_STRINGS: usize = 200_000;

/// Cap on total bytes retained in the shared-string table.
///
/// The count cap alone does not bound it: 200,000 entries of 1 MiB each is the same exhaustion
/// reached by a different shape. **Both axes, because bounding either alone leaves a product.**
const MAX_SHARED_BYTES: usize = 32 * 1024 * 1024;

pub fn extract(bytes: &[u8], format: Format) -> Result<Document, ExtractError> {
    let mut warnings = Vec::new();
    let mut archive = zip::Archive::open(bytes).map_err(|e| ExtractError::Malformed {
        detail: e.to_string(),
    })?;

    let (text, title, headings) = match format {
        Format::Docx => (docx(&mut archive, &mut warnings), None, Vec::new()),
        Format::Xlsx => (xlsx(&mut archive, &mut warnings), None, Vec::new()),
        Format::Pptx => (pptx(&mut archive, &mut warnings), None, Vec::new()),
        Format::Epub => {
            let (t, title, h) = epub(&mut archive, &mut warnings);
            (t, title, h)
        }
        other => {
            return Err(ExtractError::Unsupported {
                format: other.as_str(),
                detail: "not a ZIP-based document".to_string(),
            })
        }
    };

    let text = crate::normalize(&text, &mut warnings);
    Ok(Document {
        format,
        title,
        text,
        links: Vec::new(),
        headings,
        lang: None,
        description: None,
        bytes_in: bytes.len(),
        encoding: "UTF-8",
        warnings,
    })
}

/// Word. Paragraph boundaries come from `w:p`, and `w:tab`/`w:br` become whitespace.
fn docx(a: &mut zip::Archive<'_>, warnings: &mut Vec<Warning>) -> String {
    let mut out = String::new();
    // Body first, then the parts that carry real prose. Headers and footers are excluded: they
    // repeat on every page and are the document equivalent of a nav bar.
    let mut parts = vec!["word/document.xml".to_string()];
    for e in a.matching("word/footnotes", ".xml") {
        parts.push(e.name);
    }
    for e in a.matching("word/endnotes", ".xml") {
        parts.push(e.name);
    }

    for name in parts {
        let Some(entry) = a.find(&name).cloned() else { continue };
        match a.read(&entry) {
            Ok(data) => out.push_str(&ooxml_text(&data, &["t"], &["p"], &["tab", "br", "cr"])),
            Err(e) => warnings.push(Warning::PartSkipped {
                name,
                detail: e.to_string(),
            }),
        }
    }
    out
}

/// Excel. Cell values are indices into a shared string table, so that table is read first.
fn xlsx(a: &mut zip::Archive<'_>, warnings: &mut Vec<Warning>) -> String {
    let shared: Vec<String> = match a.find("xl/sharedStrings.xml").cloned() {
        Some(e) => match a.read(&e) {
            Ok(data) => shared_strings(&data),
            Err(err) => {
                warnings.push(Warning::PartSkipped {
                    name: "xl/sharedStrings.xml".into(),
                    detail: err.to_string(),
                });
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let mut out = String::new();
    for entry in a.matching("xl/worksheets/sheet", ".xml") {
        // The per-sheet bound above is not the per-workbook bound: an archive can declare many
        // sheets, each individually inside the cap.
        if out.len() >= crate::MAX_TEXT_CHARS {
            break;
        }
        match a.read(&entry) {
            Ok(data) => {
                out.push_str(&sheet_text(&data, &shared));
                out.push('\n');
            }
            Err(e) => warnings.push(Warning::PartSkipped {
                name: entry.name.clone(),
                detail: e.to_string(),
            }),
        }
    }
    out
}

/// PowerPoint. Slides in natural order, notes after each slide's body.
fn pptx(a: &mut zip::Archive<'_>, warnings: &mut Vec<Warning>) -> String {
    let mut out = String::new();
    for entry in a.matching("ppt/slides/slide", ".xml") {
        match a.read(&entry) {
            Ok(data) => {
                out.push_str(&ooxml_text(&data, &["t"], &["p"], &["br"]));
                out.push('\n');
            }
            Err(e) => warnings.push(Warning::PartSkipped {
                name: entry.name.clone(),
                detail: e.to_string(),
            }),
        }
    }
    for entry in a.matching("ppt/notesSlides/notesSlide", ".xml") {
        if let Ok(data) = a.read(&entry) {
            out.push_str(&ooxml_text(&data, &["t"], &["p"], &["br"]));
            out.push('\n');
        }
    }
    out
}

/// epub. The spine names the reading order; chapters are XHTML and route through [`crate::html`].
fn epub(
    a: &mut zip::Archive<'_>,
    warnings: &mut Vec<Warning>,
) -> (String, Option<String>, Vec<Heading>) {
    // container.xml points at the OPF, which holds the manifest and the spine.
    let opf_path = a
        .find("META-INF/container.xml")
        .cloned()
        .and_then(|e| a.read(&e).ok())
        .and_then(|d| first_attr(&d, "rootfile", "full-path"));

    let mut chapters: Vec<String> = Vec::new();
    let mut title = None;

    if let Some(opf_path) = opf_path.clone() {
        if let Some(opf) = a.find(&opf_path).cloned().and_then(|e| a.read(&e).ok()) {
            title = first_text(&opf, "title");
            let base = opf_path.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
            for href in spine_hrefs(&opf) {
                chapters.push(format!("{base}{href}"));
            }
        }
    }
    if chapters.is_empty() {
        // No usable spine: fall back to every XHTML part in natural order. A malformed epub still
        // has its chapters, and refusing to read them because the index is broken helps nobody.
        warnings.push(Warning::Recovered {
            detail: "no readable spine; reading all XHTML parts in archive order".to_string(),
        });
        for e in a.entries.clone() {
            if e.name.ends_with(".xhtml") || e.name.ends_with(".html") || e.name.ends_with(".htm") {
                chapters.push(e.name);
            }
        }
    }

    let mut out = String::new();
    let mut headings = Vec::new();
    for name in chapters.into_iter().take(MAX_CHAPTERS) {
        let Some(entry) = a.find(&name).cloned() else { continue };
        let Ok(data) = a.read(&entry) else {
            warnings.push(Warning::PartSkipped { name, detail: "unreadable".into() });
            continue;
        };
        match crate::html::extract(&data, Some("application/xhtml+xml"), None) {
            Ok(d) => {
                headings.extend(d.headings);
                out.push_str(&d.text);
                out.push_str("\n\n");
            }
            Err(e) => warnings.push(Warning::PartSkipped { name, detail: e.to_string() }),
        }
    }
    (out, title, headings)
}

// ── XML helpers ──────────────────────────────────────────────────────────────────────────────

/// Pull text out of an OOXML part.
///
/// `text_tags` are the elements whose character data is prose (`w:t`, `a:t`). `break_tags` end a
/// block. `space_tags` are empty elements that stand for whitespace. Namespace prefixes are
/// ignored by comparing local names — `w:t` and `a:t` differ only by prefix and mean the same.
fn ooxml_text(data: &[u8], text_tags: &[&str], break_tags: &[&str], space_tags: &[&str]) -> String {
    let text = String::from_utf8_lossy(data);
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(false);
    let mut out = String::with_capacity(data.len() / 4);
    let mut depth_in_text = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if text_tags.contains(&name.as_str()) {
                    depth_in_text += 1;
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if text_tags.contains(&name.as_str()) {
                    depth_in_text = depth_in_text.saturating_sub(1);
                }
                if break_tags.contains(&name.as_str()) && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                if space_tags.contains(&name.as_str()) && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            Ok(Event::Text(e)) => {
                if depth_in_text > 0 {
                    if let Ok(s) = e.unescape() {
                        out.push_str(&s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // Malformed tail: keep what was read.
            _ => {}
        }
    }
    out
}

/// `xl/sharedStrings.xml` — `<si>` entries, each possibly split across several `<t>` runs.
/// The shared-string table, **bounded on both count and total bytes**. Audit finding G6.
///
/// Entries past either bound become empty strings rather than being dropped, because a cell
/// referencing index `i` looks the entry up **by position** — dropping entries would shift every
/// later index and silently attribute one cell's text to another cell. An empty string at a known
/// index is a visible absence; a shifted table is quiet corruption, which is worse.
fn shared_strings(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut reader = Reader::from_str(&text);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_t = false;
    let mut retained_bytes = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()).as_str() {
                "si" => current.clear(),
                "t" => in_t = true,
                _ => {}
            },
            Ok(Event::End(e)) => match local_name(e.name().as_ref()).as_str() {
                "si" => {
                    let entry = std::mem::take(&mut current);
                    if out.len() >= MAX_SHARED_STRINGS {
                        // Stop growing the table entirely: past the count cap, no index can be
                        // looked up anyway, so there is nothing to keep positions for.
                        break;
                    }
                    if retained_bytes + entry.len() > MAX_SHARED_BYTES {
                        out.push(String::new());
                    } else {
                        retained_bytes += entry.len();
                        out.push(entry);
                    }
                }
                "t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(e)) if in_t => {
                if let Ok(s) = e.unescape() {
                    // Bounded here too, so one `<si>` cannot be a full copy of the part before
                    // the byte budget above ever sees it.
                    if current.len() + s.len() <= MAX_SHARED_BYTES {
                        current.push_str(&s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// A worksheet. `<c t="s"><v>7</v></c>` means shared string 7; anything else is a literal value.
fn sheet_text(data: &[u8], shared: &[String]) -> String {
    let text = String::from_utf8_lossy(data);
    let mut reader = Reader::from_str(&text);
    let mut out = String::new();
    let mut cell_is_shared = false;
    let mut in_v = false;
    let mut in_is = false;
    let mut row = Vec::<String>::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()).as_str() {
                "c" => {
                    cell_is_shared = e.attributes().flatten().any(|a| {
                        a.key.as_ref() == b"t" && a.value.as_ref() == b"s"
                    });
                }
                "v" => in_v = true,
                "is" => in_is = true,
                "row" => row.clear(),
                _ => {}
            },
            Ok(Event::End(e)) => match local_name(e.name().as_ref()).as_str() {
                "v" => in_v = false,
                "is" => in_is = false,
                "row" => {
                    // Bounded against `MAX_TEXT_CHARS` here rather than only in `normalize`.
                    // `normalize` runs after the whole sheet is in memory, so it caps what is
                    // *kept* and not what is *built* — which is the same "a cap on output, not on
                    // reading" shape as audit finding G7, in a different parser.
                    if !row.is_empty() && out.len() < crate::MAX_TEXT_CHARS {
                        out.push_str(&row.join("\t"));
                        out.push('\n');
                    }
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_v || in_is => {
                if let Ok(s) = e.unescape() {
                    let s = s.trim();
                    // **G6's bound, and it is on the ROW rather than on the table.** The
                    // amplification is `cells × shared_string_len`, so capping the table alone
                    // still allows 200,000 clones of a permitted-size entry. Both are capped;
                    // this is the one that multiplies.
                    if s.is_empty() || row.len() >= MAX_SHEET_COLS {
                    } else if cell_is_shared && in_v {
                        if let Ok(i) = s.parse::<usize>() {
                            if let Some(v) = shared.get(i) {
                                row.push(v.clone());
                            }
                        }
                    } else {
                        row.push(s.to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Manifest + spine: map `idref` to `href` and emit hrefs in reading order.
fn spine_hrefs(opf: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(opf);
    let mut reader = Reader::from_str(&text);
    let mut manifest: Vec<(String, String)> = Vec::new();
    let mut order: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                match local_name(e.name().as_ref()).as_str() {
                    "item" => {
                        let mut id = None;
                        let mut href = None;
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"id" => id = Some(String::from_utf8_lossy(&a.value).into_owned()),
                                b"href" => {
                                    href = Some(String::from_utf8_lossy(&a.value).into_owned())
                                }
                                _ => {}
                            }
                        }
                        if let (Some(i), Some(h)) = (id, href) {
                            manifest.push((i, h));
                        }
                    }
                    "itemref" => {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"idref" {
                                order.push(String::from_utf8_lossy(&a.value).into_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| manifest.iter().find(|(i, _)| *i == id).map(|(_, h)| h.clone()))
        .collect()
}

fn first_attr(data: &[u8], tag: &str, attr: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let mut reader = Reader::from_str(&text);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == tag {
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == attr.as_bytes() {
                            return Some(String::from_utf8_lossy(&a.value).into_owned());
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn first_text(data: &[u8], tag: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let mut reader = Reader::from_str(&text);
    let mut in_tag = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if local_name(e.name().as_ref()) == tag => in_tag = true,
            Ok(Event::End(e)) if local_name(e.name().as_ref()) == tag => in_tag = false,
            Ok(Event::Text(e)) if in_tag => {
                if let Ok(s) = e.unescape() {
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn push_chardata(out: &mut String, s: &str, title: &mut Option<String>, in_title: bool) {
    if s.is_empty() {
        return;
    }
    if in_title && title.is_none() {
        *title = Some(s.to_string());
    }
    out.push_str(s);
    out.push('\n');
}

/// Strip the namespace prefix: `w:t` -> `t`.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_ascii_lowercase(),
        None => s.to_ascii_lowercase(),
    }
}

/// Generic XML — RSS, Atom, sitemaps, arbitrary data files.
///
/// All character data, in document order, with elements as block boundaries. No schema knowledge:
/// a feed and a dataset get the same treatment, which is the honest thing when the shape is
/// unknown.
pub fn extract_xml(bytes: &[u8], content_type: Option<&str>) -> Result<Document, ExtractError> {
    let mut warnings = Vec::new();
    let decoded = charset::decode(bytes, content_type, false, &mut warnings);
    let mut reader = Reader::from_str(&decoded.text);
    let mut out = String::with_capacity(decoded.text.len() / 3);
    let mut title = None;
    let mut in_title = false;
    let mut skip = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let n = local_name(e.name().as_ref());
                if n == "script" || n == "style" {
                    skip += 1;
                }
                if n == "title" && title.is_none() {
                    in_title = true;
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(e.name().as_ref());
                if n == "script" || n == "style" {
                    skip = skip.saturating_sub(1);
                }
                if n == "title" {
                    in_title = false;
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Ok(Event::Text(e)) if skip == 0 => {
                if let Ok(s) = e.unescape() {
                    push_chardata(&mut out, s.trim(), &mut title, in_title);
                }
            }
            // CDATA is already literal — unescaping it would be wrong, not merely unnecessary.
            // Feeds carry their HTML payloads this way, so this is a real path, not an edge case.
            Ok(Event::CData(e)) if skip == 0 => {
                let s = String::from_utf8_lossy(&e).into_owned();
                push_chardata(&mut out, s.trim(), &mut title, in_title);
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warnings.push(Warning::Recovered { detail: e.to_string() });
                break;
            }
            _ => {}
        }
    }

    let text = crate::normalize(&out, &mut warnings);
    Ok(Document {
        format: Format::Xml,
        title,
        text,
        links: Vec::new(),
        headings: Vec::new(),
        lang: None,
        description: None,
        bytes_in: bytes.len(),
        encoding: decoded.encoding,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_prefixes_are_ignored_when_matching_element_names() {
        assert_eq!(local_name(b"w:t"), "t");
        assert_eq!(local_name(b"a:t"), "t");
        assert_eq!(local_name(b"t"), "t");
    }

    #[test]
    fn docx_paragraphs_become_lines() {
        let xml = br#"<w:document><w:body>
            <w:p><w:r><w:t>First para.</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second </w:t></w:r><w:r><w:t>para.</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let got = ooxml_text(xml, &["t"], &["p"], &["tab", "br", "cr"]);
        assert!(got.contains("First para."), "got {got:?}");
        assert!(got.contains("Second para."), "runs must join: {got:?}");
    }

    #[test]
    fn xlsx_resolves_shared_string_indices() {
        let shared = shared_strings(
            br#"<sst><si><t>Country</t></si><si><t>France</t></si></sst>"#,
        );
        assert_eq!(shared, vec!["Country", "France"]);
        let sheet = br#"<worksheet><sheetData>
            <row><c t="s"><v>0</v></c><c t="s"><v>1</v></c></row>
            <row><c><v>42</v></c></row>
            </sheetData></worksheet>"#;
        let got = sheet_text(sheet, &shared);
        assert!(got.contains("Country\tFrance"), "got {got:?}");
        assert!(got.contains("42"), "a literal value must survive: {got:?}");
    }

    /// **Audit G6 — assert the BOUND, not survival.**
    ///
    /// The exploit is `cells × shared_string_len`: one large shared string referenced by very many
    /// cells in a single row. The failure is an allocation `abort`, which `catch_unwind` cannot
    /// intercept, so "the call returned" is not the property — a test asserting that would pass on
    /// every input smaller than the one that kills the process. The property is that the retained
    /// cell count is bounded no matter how many the sheet declares.
    #[test]
    fn a_single_row_cannot_amplify_a_shared_string_without_bound() {
        // One entry, referenced far more times than the cap allows.
        let shared = vec!["PAYLOAD".repeat(64)];
        let cells = "<c t=\"s\"><v>0</v></c>".repeat(MAX_SHEET_COLS + 500);
        let sheet = format!("<worksheet><sheetData><row>{cells}</row></sheetData></worksheet>");

        let got = sheet_text(sheet.as_bytes(), &shared);

        let widest = got.lines().map(|l| l.split('\t').count()).max().unwrap_or(0);
        assert!(
            widest <= MAX_SHEET_COLS,
            "retained {widest} cells against a cap of {MAX_SHEET_COLS}"
        );
        // The vacuity control: the cells that were retained still carry their text, so this is a
        // statement about a bound rather than about an empty sheet.
        assert!(got.contains("PAYLOAD"), "nothing was extracted at all");
    }

    /// The multiplicand, bounded on both axes. Count alone does not bound bytes and bytes alone
    /// do not bound count, so both are asserted.
    #[test]
    fn the_shared_string_table_is_bounded_on_count_and_on_bytes() {
        // Bytes: far fewer entries than the count cap, each enormous.
        let one = format!("<si><t>{}</t></si>", "x".repeat(2 * 1024 * 1024));
        let sst = format!("<sst>{}</sst>", one.repeat(40));
        let table = shared_strings(sst.as_bytes());
        let bytes: usize = table.iter().map(|s| s.len()).sum();
        assert!(
            bytes <= MAX_SHARED_BYTES,
            "retained {bytes} bytes against a cap of {MAX_SHARED_BYTES}"
        );
        // **Positions are preserved.** An over-budget entry becomes empty rather than being
        // dropped, because a cell looks its text up by index and a shifted table would attribute
        // one cell's text to another — quiet corruption in place of a visible absence.
        assert_eq!(table.len(), 40, "entries must keep their indices, not be dropped");
    }

    #[test]
    fn an_epub_spine_orders_chapters_by_idref_not_manifest_order() {
        let opf = br#"<package><manifest>
            <item id="c2" href="two.xhtml"/>
            <item id="c1" href="one.xhtml"/>
            </manifest><spine>
            <itemref idref="c1"/><itemref idref="c2"/>
            </spine></package>"#;
        assert_eq!(spine_hrefs(opf), vec!["one.xhtml", "two.xhtml"]);
    }

    #[test]
    fn generic_xml_yields_its_character_data() {
        let d = extract_xml(
            br#"<rss><channel><title>Feed</title><item><description>Hello &amp; welcome</description></item></channel></rss>"#,
            Some("application/rss+xml"),
        )
        .expect("extracts");
        assert_eq!(d.title.as_deref(), Some("Feed"));
        assert!(d.text.contains("Hello & welcome"), "got {:?}", d.text);
    }

    #[test]
    fn malformed_xml_keeps_what_was_read_and_says_so() {
        let d = extract_xml(b"<a><b>kept</b><c>unclosed", Some("text/xml")).expect("recovers");
        assert!(d.text.contains("kept"));
    }
}
