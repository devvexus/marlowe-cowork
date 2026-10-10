//! Papers as structure: title, abstract, numbered sections and a parsed reference list.
//!
//! Three readers share one [`Paper`] shape:
//! - [`from_html`]: arXiv's LaTeXML HTML (`arxiv.org/html/<id>`) and other HTML papers, read
//!   through html.rs blocks (headings become sections, the bibliography's list items references).
//! - [`latex::from_source`]: an arXiv e-print (gzip, tar or a single `.tex`), with `.bbl`,
//!   `thebibliography` or `.bib` references.
//! - [`from_text`]: PDF text, with sections and the reference list recovered heuristically.
//!
//! No network here; the caller fetches. Everything parsed is untrusted text.

pub mod latex;
mod tar;

use std::sync::OnceLock;

use regex::Regex;

use crate::main_content::{to_markdown, BlockKind, BlockSpan};
use crate::{Document, Link};

/// Caps on what a paper may yield.
pub const MAX_REFERENCES: usize = 1_000;
pub const MAX_SECTIONS: usize = 200;
const MAX_REFERENCE_CHARS: usize = 1_500;

/// Where a paper's text came from, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PaperSource {
    ArxivHtml,
    Html,
    Latex,
    Pdf,
}

impl PaperSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PaperSource::ArxivHtml => "arxiv-html",
            PaperSource::Html => "html",
            PaperSource::Latex => "latex",
            PaperSource::Pdf => "pdf",
        }
    }
}

/// One section heading and where its text starts in [`Paper::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub level: u8,
    /// `"2.1"` when the heading is numbered.
    pub number: Option<String>,
    pub title: String,
    /// Byte offset of the heading in [`Paper::text`].
    pub offset: usize,
}

/// One parsed reference. Identifiers are as printed, normalised lightly (DOI lowercased, arXiv
/// id without version); the dedup layer decides what they mean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    /// `"1"` for `[1]`, or a cite key from LaTeX.
    pub label: Option<String>,
    pub text: String,
    pub title: Option<String>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub url: Option<String>,
    pub year: Option<u16>,
}

/// A paper read for research.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paper {
    pub source: PaperSource,
    pub title: Option<String>,
    pub abstract_text: Option<String>,
    /// Body as markdown (HTML, LaTeX) or plain text (PDF); references are not repeated here.
    pub text: String,
    pub sections: Vec<Section>,
    pub references: Vec<Reference>,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn doi_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)\b(10\.\d{4,9}/[^\s<>]+)")
}

fn arxiv_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)(?:arxiv[:\s]\s*|arxiv\.org/(?:abs|pdf|html)/|10\.48550/arxiv\.)(\d{4}\.\d{4,5}|[a-z\-]+(?:\.[a-z]{2})?/\d{7})",
    )
}

fn year_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\b(19[5-9]\d|20[0-4]\d)\b")
}

fn label_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s*\[(\w{1,24})\]\s*|^\s*(\d{1,4})\.\s+")
}

fn heading_number_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^((?:\d{1,2})(?:\.\d{1,2}){0,3}|[IVX]{1,4})\.?\s+(\S.*)$")
}

fn is_reference_heading(t: &str) -> bool {
    let t = t.trim().trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ').to_ascii_lowercase();
    matches!(t.as_str(), "references" | "bibliography" | "works cited" | "literature cited" | "references and notes")
}

fn split_number(heading: &str) -> (Option<String>, String) {
    match heading_number_re().captures(heading.trim()) {
        Some(c) => (Some(c[1].to_string()), c[2].trim().to_string()),
        None => (None, heading.trim().to_string()),
    }
}

fn trim_id(s: &str) -> &str {
    s.trim_end_matches(['.', ',', ';', ')', ']', '}', '\'', '"'])
}

/// Parses one reference's text (and the links found inside it, if any).
pub fn reference_from_text(raw: &str, links: &[&Link]) -> Reference {
    static GLUED: OnceLock<Regex> = OnceLock::new();
    // Adjacent inline blocks (LaTeXML bibblocks) arrive as "al.Attention"; split them back.
    let raw = re(&GLUED, r"([\p{Ll}\d\)\]]\.)(\p{Lu})").replace_all(raw, "$1 $2");
    let mut text: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > MAX_REFERENCE_CHARS {
        text.truncate(crate::floor_char_boundary(&text, MAX_REFERENCE_CHARS));
    }
    let mut r = Reference::default();
    if let Some(c) = label_re().captures(&text) {
        r.label = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string());
        let end = c.get(0).map_or(0, |m| m.end());
        text = text[end..].trim().to_string();
    }
    let hay: Vec<&str> = std::iter::once(text.as_str()).chain(links.iter().map(|l| l.url.as_str())).collect();
    for h in &hay {
        if r.doi.is_none() {
            r.doi = doi_re().captures(h).map(|c| trim_id(&c[1]).to_ascii_lowercase());
        }
        if r.arxiv_id.is_none() {
            r.arxiv_id = arxiv_re().captures(h).map(|c| c[1].to_string());
        }
    }
    r.url = links
        .iter()
        .map(|l| l.url.clone())
        .chain(
            text.split_whitespace()
                .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
                .map(|w| trim_id(w).to_string()),
        )
        .next();
    r.year = year_re().find_iter(&text).last().and_then(|m| m.as_str().parse().ok());
    r.title = guess_title(&text);
    r.text = text;
    r
}

/// The first sentence after the author list in "Authors. Title. Venue, year.".
fn guess_title(text: &str) -> Option<String> {
    let names_only = |p: &str| p.split_whitespace().all(|w| w == "and" || w.starts_with(char::is_uppercase));
    let authorish =
        |p: &str| p.contains("et al") || p.contains(',') || !p.contains(' ') || (p.contains(" and ") && names_only(p));
    let venue = |p: &str| {
        let l = p.to_ascii_lowercase();
        ["in ", "proc", "arxiv", "advances in", "journal", "transactions", "doi", "http"]
            .iter()
            .any(|v| l.starts_with(v))
            || year_re().is_match(p) && p.split_whitespace().count() <= 3
    };
    let mut seen_author = false;
    for p in text.split(". ").map(|p| p.trim().trim_end_matches('.')).filter(|p| !p.is_empty()) {
        if authorish(p) {
            seen_author = true;
            continue;
        }
        if seen_author && !venue(p) && p.split_whitespace().count() >= 2 && p.len() <= 300 {
            return Some(p.to_string());
        }
        if seen_author {
            return None;
        }
    }
    None
}

/// Reads an HTML paper from html.rs's structured output (see [`crate::html::extract_structured`]).
/// `arxiv` marks arXiv's LaTeXML pages.
pub fn from_html(doc: &Document, blocks: &[BlockSpan], arxiv: bool) -> Paper {
    let mut body: Vec<usize> = Vec::new();
    let mut references = Vec::new();
    let mut title = None;
    let mut abstract_text: Option<String> = None;
    let mut in_refs = false;
    let mut in_abstract = false;
    // Inside <article>/<main> when the page marks one (captions there are content), else not chrome.
    let marked = blocks.iter().any(|b| b.content);
    for (i, b) in blocks.iter().enumerate() {
        let visible = if marked { b.content } else { !b.boilerplate };
        if !visible {
            continue;
        }
        if let BlockKind::Heading(level) = b.kind {
            in_refs = is_reference_heading(&b.text);
            in_abstract = b.text.trim().eq_ignore_ascii_case("abstract");
            if level == 1 && title.is_none() {
                // The title heading is the paper's title, not a section.
                title = Some(b.text.trim().to_string());
                continue;
            }
            if in_refs || in_abstract {
                continue;
            }
        } else if in_refs {
            if references.len() < MAX_REFERENCES && matches!(b.kind, BlockKind::ListItem | BlockKind::Paragraph) {
                let links: Vec<&Link> = b.links.iter().filter_map(|&l| doc.links.get(l)).collect();
                references.push(reference_from_text(&b.text, &links));
            }
            continue;
        } else if in_abstract {
            let a = abstract_text.get_or_insert_with(String::new);
            if !a.is_empty() {
                a.push('\n');
            }
            a.push_str(b.text.trim());
            continue;
        }
        body.push(i);
    }
    let text = to_markdown(blocks, &body, &doc.links);
    let sections = sections_from_markdown(&text);
    Paper {
        source: if arxiv { PaperSource::ArxivHtml } else { PaperSource::Html },
        title: title.or_else(|| doc.title.clone()),
        abstract_text,
        text,
        sections,
        references,
    }
}

/// Sections from markdown `#` headings, numbered when the heading starts with a number.
pub fn sections_from_markdown(md: &str) -> Vec<Section> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut fence = false;
    for line in md.split_inclusive('\n') {
        let l = line.trim_end();
        if l.starts_with("```") {
            fence = !fence;
        }
        let hashes = l.bytes().take_while(|b| *b == b'#').count();
        if !fence && (1..=6).contains(&hashes) && l.as_bytes().get(hashes) == Some(&b' ') && out.len() < MAX_SECTIONS {
            let (number, title) = split_number(&l[hashes + 1..]);
            out.push(Section { level: hashes as u8, number, title, offset });
        }
        offset += line.len();
    }
    out
}

/// Sections and references recovered from plain text (PDF): numbered or standard headings on
/// their own line; the reference list after the last "References" line.
pub fn from_text(text: &str, title: Option<String>) -> Paper {
    static STD: OnceLock<Regex> = OnceLock::new();
    let std_heading = re(
        &STD,
        r"(?i)^(abstract|introduction|related work|background|preliminaries|methods?|methodology|approach|experiments?|evaluation|results|discussion|conclusions?|limitations|acknowledg(?:e)?ments|references|bibliography|appendix(?: [a-z])?)$",
    );
    let mut sections = Vec::new();
    let mut refs_at = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let l = line.trim();
        let numbered = heading_number_re().captures(l).filter(|c| {
            c[2].chars().next().is_some_and(char::is_uppercase) && c[2].len() <= 80 && !c[2].ends_with('.')
        });
        if l.len() <= 90 && (numbered.is_some() || std_heading.is_match(l)) && sections.len() < MAX_SECTIONS {
            let (number, t) = split_number(l);
            let level = number.as_deref().map_or(1, |n| 1 + n.matches('.').count() as u8);
            if is_reference_heading(&t) {
                refs_at = Some(offset + line.len());
            }
            if !sections.iter().any(|s: &Section| s.title == t && s.number == number) {
                sections.push(Section { level, number, title: t, offset });
            }
        }
        offset += line.len();
    }
    let (body, references) = match refs_at {
        Some(at) => (text[..at].to_string(), references_from_text(&text[at..])),
        None => (text.to_string(), Vec::new()),
    };
    let abstract_text = sections.iter().position(|s| s.title.eq_ignore_ascii_case("abstract")).map(|i| {
        let start = sections[i].offset;
        let end = sections.get(i + 1).map_or(body.len().min(start + 3_000), |s| s.offset.min(body.len()));
        body.get(start..end.max(start)).unwrap_or("").lines().skip(1).collect::<Vec<_>>().join(" ").trim().to_string()
    });
    Paper { source: PaperSource::Pdf, title, abstract_text, text: body, sections, references }
}

/// Splits a reference list: at `[n]` / `n.` starts when the list uses them, else at blank lines.
pub fn references_from_text(list: &str) -> Vec<Reference> {
    static START: OnceLock<Regex> = OnceLock::new();
    let start = re(&START, r"^\s*(\[\w{1,24}\]|\d{1,4}\.\s)");
    let lines: Vec<&str> = list.lines().collect();
    let numbered = lines.iter().filter(|l| start.is_match(l)).count() >= 2;
    let mut entries: Vec<String> = Vec::new();
    let mut cur = String::new();
    for l in lines {
        let t = l.trim();
        let boundary = if numbered { start.is_match(l) } else { t.is_empty() };
        if boundary && !cur.trim().is_empty() {
            entries.push(std::mem::take(&mut cur));
        }
        if !t.is_empty() {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(t);
        }
        if entries.len() >= MAX_REFERENCES {
            break;
        }
    }
    if !cur.trim().is_empty() && entries.len() < MAX_REFERENCES {
        entries.push(cur);
    }
    entries.iter().filter(|e| e.len() >= 20).map(|e| reference_from_text(e, &[])).collect()
}

#[cfg(test)]
mod tests;
