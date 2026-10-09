//! Text-shaped formats: plain text, Markdown, JSON, CSV.
//!
//! These share one property that shapes the whole module: **the characters are already the
//! content.** There is no markup to strip, so the job is decoding, light structural flattening,
//! and — for the two structured formats — turning a machine shape into something a reader (or a
//! model, or an embedder) can use as prose.
//!
//! Flattening JSON and CSV rather than pretty-printing them is the deliberate part. A research
//! agent that fetches a dataset wants *"country: France, population: 68000000"*, not two hundred
//! lines of punctuation, because every downstream stage — ranking, embedding, summarising —
//! scores the punctuation too.

use crate::{charset, Document, ExtractError, Format, Heading, Warning};

/// How many rows of a CSV get flattened into prose before the rest is summarised.
///
/// A cap on *output*, not on reading: the row count reported afterwards is the true one. Without
/// this a 400 MB export becomes 400 MB of `col: value` text, which is worse than useless — it is
/// expensive and useless.
const MAX_CSV_ROWS: usize = 5_000;

/// Cap on retained columns per CSV row. See [`parse_csv`] — a single very wide row is the same
/// exhaustion as very many rows, and it never trips the row cap.
const MAX_CSV_COLS: usize = 4_096;

/// Cap on one CSV field. A file containing no delimiter and no newline is otherwise a full second
/// copy of the input held alongside it.
const MAX_CSV_FIELD_BYTES: usize = 1024 * 1024;

/// Depth limit for JSON flattening. Beyond this a value is summarised by its shape.
const MAX_JSON_DEPTH: usize = 24;

pub fn extract(
    bytes: &[u8],
    format: Format,
    content_type: Option<&str>,
) -> Result<Document, ExtractError> {
    let mut warnings = Vec::new();
    let decoded = charset::decode(bytes, content_type, false, &mut warnings);

    let (text, headings) = match format {
        Format::Markdown => markdown(&decoded.text),
        Format::Json => (json(&decoded.text, &mut warnings), Vec::new()),
        Format::Csv => (csv(&decoded.text, &mut warnings), Vec::new()),
        _ => (decoded.text.clone(), Vec::new()),
    };

    let title = match format {
        Format::Markdown => headings.iter().find(|h| h.level == 1).map(|h| h.text.clone()),
        _ => None,
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
        encoding: decoded.encoding,
        warnings,
    })
}

/// Markdown is kept nearly verbatim — it is already the readable form.
///
/// Only the headings are lifted out into structure, because a caller that wants an outline should
/// not have to re-parse the text to get one. Fences are tracked so a `#` in a code block is not
/// mistaken for a heading.
fn markdown(src: &str) -> (String, Vec<Heading>) {
    let mut headings = Vec::new();
    let mut in_fence = false;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = t.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) && t.as_bytes().get(hashes) == Some(&b' ') {
            let text = t[hashes + 1..].trim().trim_end_matches('#').trim().to_string();
            if !text.is_empty() {
                headings.push(Heading { level: hashes as u8, text });
            }
        }
    }
    (src.to_string(), headings)
}

/// Flatten JSON into `path: value` lines.
///
/// Hand-rolled rather than pulled through `serde_json`, for one reason worth stating: a research
/// corpus is full of **almost**-JSON — trailing commas, NDJSON, truncated downloads — and a strict
/// parser returns nothing at all for any of it. This walks the text and emits what it can, so a
/// malformed file yields its readable content instead of an error.
fn json(src: &str, warnings: &mut Vec<Warning>) -> String {
    let mut out = String::with_capacity(src.len() / 2);
    let mut depth = 0usize;
    let mut chars = src.char_indices().peekable();
    let mut pending_key: Option<String> = None;
    let mut deepest = 0usize;

    while let Some((i, c)) = chars.next() {
        match c {
            '{' | '[' => {
                depth += 1;
                deepest = deepest.max(depth);
                if depth > MAX_JSON_DEPTH {
                    continue;
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            '"' => {
                let (s, next) = read_json_string(src, i);
                // **Advance by POSITION, not by a byte count used as a step count.**
                //
                // `next - i - 1` is a byte distance; using it as a number of `char_indices` steps
                // over-advanced on any non-ASCII string and silently swallowed the following
                // fields. A dataset in any non-English language lost data with no warning --
                // "silently wrong", which is the failure `charset.rs` exists to prevent.
                while chars.peek().is_some_and(|(j, _)| *j < next) {
                    chars.next();
                }
                // A string followed by `:` is a key; anything else is a value.
                let is_key = src[next..].trim_start().starts_with(':');
                if is_key {
                    pending_key = Some(s);
                } else {
                    emit(&mut out, pending_key.take(), &s);
                }
            }
            c if c.is_ascii_digit() || c == '-' => {
                let start = i;
                let mut end = i + c.len_utf8();
                while let Some((j, d)) = chars.peek().copied() {
                    if d.is_ascii_digit() || d == '.' || d == 'e' || d == 'E' || d == '+' || d == '-'
                    {
                        end = j + d.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                emit(&mut out, pending_key.take(), &src[start..end]);
            }
            't' | 'f' | 'n' => {
                for word in ["true", "false", "null"] {
                    if src[i..].starts_with(word) {
                        for _ in 1..word.len() {
                            chars.next();
                        }
                        emit(&mut out, pending_key.take(), word);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if deepest > MAX_JSON_DEPTH {
        warnings.push(Warning::Recovered {
            detail: format!("JSON nested {deepest} deep; below {MAX_JSON_DEPTH} was flattened"),
        });
    }
    out
}

fn emit(out: &mut String, key: Option<String>, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    if let Some(k) = key {
        out.push_str(&k);
        out.push_str(": ");
    }
    out.push_str(value);
    out.push('\n');
}

/// Read a JSON string starting at the opening quote. Returns the unescaped value and the index
/// just past the closing quote.
fn read_json_string(src: &str, open: usize) -> (String, usize) {
    let bytes = src.as_bytes();
    let mut out = String::new();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return (out, i + 1),
            b'\\' if i + 1 < bytes.len() => {
                let e = bytes[i + 1];
                match e {
                    b'n' | b'r' => out.push(' '),
                    b't' => out.push(' '),
                    b'u' => {
                        if let Some(hex) = src.get(i + 2..i + 6) {
                            if let Some(c) =
                                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                            {
                                out.push(c);
                            }
                            i += 6;
                            continue;
                        }
                    }
                    other => {
                        // **Decode the whole character before advancing.**
                        //
                        // `i += 2` stepped one byte past the backslash and one byte INTO a
                        // multi-byte escaped character, after which the `_` arm sliced at a
                        // non-boundary and panicked. `["\<e-acute>"]` was enough.
                        let ch = src[i + 1..].chars().next().unwrap_or(other as char);
                        out.push(ch);
                        i += 1 + ch.len_utf8();
                        continue;
                    }
                }
                i += 2;
                continue;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
    }
    (out, bytes.len())
}

/// Flatten CSV into `header: cell` prose, one record per block.
///
/// RFC 4180 quoting is honoured — a delimiter inside quotes is data, and `""` is a literal quote —
/// because a naive `split(',')` corrupts exactly the rows that contain prose, which are the rows a
/// research agent cares about.
fn csv(src: &str, warnings: &mut Vec<Warning>) -> String {
    let delim = pick_delimiter(src);
    // `total_rows` is the TRUE count even though only `MAX_CSV_ROWS` were retained — the
    // truncation notice below reports it, and reporting the retained count instead would make a
    // 5-million-row export read as a 5,000-row one.
    let (rows, total_rows) = parse_csv(src, delim, MAX_CSV_ROWS);
    if rows.is_empty() {
        return String::new();
    }
    let header = &rows[0];
    let looks_headed = header.iter().all(|c| !c.trim().is_empty())
        && header.iter().any(|c| c.parse::<f64>().is_err());

    let mut out = String::new();
    let body = if looks_headed { &rows[1..] } else { &rows[..] };
    for row in body.iter().take(MAX_CSV_ROWS) {
        for (i, cell) in row.iter().enumerate() {
            if cell.trim().is_empty() {
                continue;
            }
            if looks_headed {
                if let Some(h) = header.get(i) {
                    out.push_str(h.trim());
                    out.push_str(": ");
                }
            }
            out.push_str(cell.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    // The body's true length: `total_rows` counts every row in the file, and the header is one of
    // them when there is one.
    let total_body = if looks_headed { total_rows.saturating_sub(1) } else { total_rows };
    if total_body > MAX_CSV_ROWS {
        // The true count, stated. A silently-capped export reads as a small dataset.
        out.push_str(&format!("[{MAX_CSV_ROWS} of {total_body} rows shown]\n"));
        warnings.push(Warning::Truncated { kept: MAX_CSV_ROWS, limit: MAX_CSV_ROWS });
    }
    out
}

fn pick_delimiter(src: &str) -> char {
    let head: String = src.lines().take(5).collect::<Vec<_>>().join("\n");
    [',', '\t', ';', '|']
        .into_iter()
        .max_by_key(|d| head.matches(*d).count())
        .unwrap_or(',')
}

/// Parse CSV, **retaining at most `keep` rows while counting all of them**.
///
/// # Audit finding G7 — the cap applied to the output and not to the reading
///
/// `MAX_CSV_ROWS` was applied by `.take(MAX_CSV_ROWS)` *after* this function had already built a
/// `Vec<Vec<String>>` for the entire file. The comment said so in as many words — *"a cap on
/// output, not on reading"* — which is a true description of a memory exhaustion. 32 MiB of `a,\n`
/// is ~5.5M rows; each is a `Vec` (24 bytes) holding a `String` (24 bytes plus its allocation), so
/// the cap took effect somewhere north of 2 GB, times the `rayon` fan-out across a corpus.
///
/// **Allocation failure in Rust is an `abort`, not a panic**, so the `catch_unwind` in `extract`
/// cannot intercept it: one hostile CSV takes the daemon down rather than the document.
///
/// The returned count is still the **true** row count, because that is what the truncation warning
/// reports and a silently-capped export reads as a small dataset. Counting is free; retaining is
/// what costs.
fn parse_csv(src: &str, delim: char, keep: usize) -> (Vec<Vec<String>>, usize) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut total = 0usize;
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = src.chars().peekable();

    // Retaining one more row than the caller will render is deliberate: `csv` decides whether the
    // first row is a header, so the body is `rows[1..]` and a `keep` of exactly N would render
    // N-1 rows.
    let keep = keep.saturating_add(1);

    // A row is finished: count it always, retain it only while there is room.
    macro_rules! finish_row {
        () => {{
            total += 1;
            if rows.len() < keep {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        }};
    }

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    push_field_char(&mut field, '"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                push_field_char(&mut field, c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            // **The column cap is the other half of G6/G7.** A single row of 16M `a,` fields is
            // the same exhaustion with the axes swapped, and it never reaches the row cap at all.
            c if c == delim => {
                if row.len() < MAX_CSV_COLS {
                    row.push(std::mem::take(&mut field));
                } else {
                    field.clear();
                }
            }
            '\n' => {
                if row.len() < MAX_CSV_COLS {
                    row.push(std::mem::take(&mut field));
                } else {
                    field.clear();
                }
                finish_row!();
            }
            '\r' => {}
            _ => push_field_char(&mut field, c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        if row.len() < MAX_CSV_COLS {
            row.push(std::mem::take(&mut field));
        }
        finish_row!();
    }
    (rows, total)
}

/// Append to a CSV field, bounded.
///
/// The third axis: a file with no delimiter and no newline is one field, and without this it is a
/// full second copy of the input. Bounded rather than refused — a long cell is legitimate data and
/// truncating it loses less than failing the document.
fn push_field_char(field: &mut String, c: char) {
    if field.len() < MAX_CSV_FIELD_BYTES {
        field.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(bytes: &[u8], f: Format) -> Document {
        extract(bytes, f, Some("text/plain; charset=utf-8")).expect("extracts")
    }

    #[test]
    fn markdown_keeps_its_text_and_lifts_an_outline() {
        let d = go(b"# Title\n\nSome prose.\n\n## Section\n\nMore.\n", Format::Markdown);
        assert_eq!(d.title.as_deref(), Some("Title"));
        assert_eq!(d.headings.len(), 2);
        assert!(d.text.contains("Some prose."));
    }

    #[test]
    fn a_hash_inside_a_code_fence_is_not_a_heading() {
        let d = go(b"# Real\n\n```\n# not a heading\n```\n", Format::Markdown);
        assert_eq!(d.headings.len(), 1);
    }

    #[test]
    fn json_flattens_to_readable_pairs() {
        let d = go(br#"{"country":"France","population":68000000,"eu":true}"#, Format::Json);
        assert!(d.text.contains("country: France"), "got {:?}", d.text);
        assert!(d.text.contains("population: 68000000"), "got {:?}", d.text);
        assert!(d.text.contains("eu: true"), "got {:?}", d.text);
    }

    #[test]
    fn malformed_json_still_yields_its_content() {
        // A strict parser returns nothing here; a truncated download is common in a corpus.
        let d = go(br#"{"a":"kept","b":"also kept","c":"#, Format::Json);
        assert!(d.text.contains("kept"), "got {:?}", d.text);
    }

    #[test]
    fn csv_uses_headers_and_honours_quoting() {
        let d = go(
            b"name,note\nAda,\"Lovelace, Countess\"\nAlan,\"He said \"\"hi\"\"\"\n",
            Format::Csv,
        );
        assert!(d.text.contains("name: Ada"), "got {:?}", d.text);
        assert!(
            d.text.contains("note: Lovelace, Countess"),
            "a quoted delimiter must stay data: {:?}",
            d.text
        );
        assert!(d.text.contains(r#"He said "hi""#), "got {:?}", d.text);
    }

    #[test]
    fn a_tab_separated_file_is_detected_by_delimiter_frequency() {
        let d = go(b"a\tb\n1\t2\n3\t4\n", Format::Csv);
        assert!(d.text.contains("a: 1"), "got {:?}", d.text);
    }

    /// **Audit G7 — assert the BOUND, not survival.**
    ///
    /// An allocation failure is an `abort`, so a test that merely calls this and checks it
    /// returned would pass right up until the row count that kills the process. The property is
    /// that the retained row count is bounded regardless of the input's, so that is what is
    /// asserted — together with the true count still being reported, since a cap that lies about
    /// the size of the export is a different defect.
    #[test]
    fn parse_csv_retains_a_bounded_number_of_rows_and_still_counts_them_all() {
        let src = "a,b\n".repeat(MAX_CSV_ROWS * 3);
        let (rows, total) = parse_csv(&src, ',', MAX_CSV_ROWS);
        assert!(
            rows.len() <= MAX_CSV_ROWS + 1,
            "retained {} rows against a cap of {}",
            rows.len(),
            MAX_CSV_ROWS
        );
        assert_eq!(total, MAX_CSV_ROWS * 3, "the true row count must survive the cap");
    }

    /// The other axis: one row, very many columns, never reaches the row cap at all.
    #[test]
    fn parse_csv_bounds_the_width_of_a_single_row() {
        let src = "a,".repeat(MAX_CSV_COLS * 2);
        let (rows, _) = parse_csv(&src, ',', MAX_CSV_ROWS);
        assert_eq!(rows.len(), 1, "one row expected");
        assert!(
            rows[0].len() <= MAX_CSV_COLS,
            "retained {} columns against a cap of {MAX_CSV_COLS}",
            rows[0].len()
        );
    }

    /// The third axis: no delimiter and no newline is otherwise a whole second copy of the input.
    #[test]
    fn parse_csv_bounds_a_single_enormous_field() {
        let src = "x".repeat(MAX_CSV_FIELD_BYTES + 50_000);
        let (rows, _) = parse_csv(&src, ',', MAX_CSV_ROWS);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0][0].len() <= MAX_CSV_FIELD_BYTES,
            "field was {} bytes",
            rows[0][0].len()
        );
    }

    /// The truncation notice reports the TRUE total, not the retained one — a 15,000-row export
    /// capped to 5,000 must not read as a 5,000-row export.
    #[test]
    fn the_truncation_notice_states_the_real_row_count() {
        let mut warnings = Vec::new();
        let src = format!("h1,h2\n{}", "a,b\n".repeat(MAX_CSV_ROWS * 3));
        let out = csv(&src, &mut warnings);
        assert!(out.contains(&format!("of {} rows shown", MAX_CSV_ROWS * 3)), "got: {out:?}");
    }
}
