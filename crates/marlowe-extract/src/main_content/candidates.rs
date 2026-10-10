//! The two trafilatura ports as [`MainContent`] candidates. Each is behind a cargo feature of the
//! same name; without it the type still exists and reports `Unavailable`.

// Used only when a candidate's feature is on.
#[allow(unused_imports)]
use std::time::Instant;

#[allow(unused_imports)]
use super::{Candidate, Extracted, MainContent, MainContentError, PageInput, PageType};

/// Elements dropped before a DOM parse: their contents are never main content, and on modern
/// pages they are most of the bytes (bundles, inline CSS, icon sprites).
const HEAVY: [&[u8]; 6] = [b"script", b"style", b"svg", b"template", b"iframe", b"noscript"];

/// `html` without comments and [`HEAVY`] elements, keeping JSON-LD scripts (metadata and page
/// type signals). A linear byte scan; returns the input unchanged when nothing is removed.
pub fn lighten(html: &str) -> std::borrow::Cow<'_, str> {
    let b = html.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while let Some(rel) = memchr::memchr(b'<', &b[i..]) {
        let at = i + rel;
        let rest = &b[at..];
        let skip_to = if rest.starts_with(b"<!--") {
            Some(find(b, at + 4, b"-->").map_or(b.len(), |e| e + 3))
        } else {
            HEAVY.iter().find_map(|name| {
                let n = name.len();
                let opens = rest.len() > n + 1
                    && rest[1..=n].eq_ignore_ascii_case(name)
                    && (rest[n + 1].is_ascii_whitespace() || rest[n + 1] == b'>' || rest[n + 1] == b'/');
                if !opens {
                    return None;
                }
                let tag_end = memchr::memchr(b'>', rest).map_or(b.len(), |e| at + e + 1);
                if *name == b"script" {
                    let tag = html[at..tag_end].to_ascii_lowercase();
                    if tag.contains("ld+json") {
                        return None;
                    }
                }
                let close = [b"</".as_slice(), name].concat();
                Some(
                    find_ci(b, tag_end, &close)
                        .map_or(b.len(), |e| memchr::memchr(b'>', &b[e..]).map_or(b.len(), |g| e + g + 1)),
                )
            })
        };
        match skip_to {
            Some(end) => {
                let o = out.get_or_insert_with(|| String::with_capacity(html.len() / 2));
                o.push_str(&html[copied..at]);
                copied = end.min(html.len());
                i = copied;
            }
            None => i = at + 1,
        }
        if i >= b.len() {
            break;
        }
    }
    match out {
        Some(mut o) => {
            o.push_str(&html[copied..]);
            std::borrow::Cow::Owned(o)
        }
        None => std::borrow::Cow::Borrowed(html),
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(hay.get(from..)?, needle).map(|i| from + i)
}

fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let first = needle[0];
    let mut i = from;
    while i < hay.len() {
        let at = i + memchr::memchr(first, &hay[i..])?;
        if hay.len() - at >= needle.len() && hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// The page type the URL alone suggests (rs-trafilatura's URL heuristics; forums and docs are
/// recognised with high precision). `Unknown` without the feature.
pub fn url_page_type(url: &str) -> PageType {
    #[cfg(feature = "rs-trafilatura")]
    {
        PageType::parse(rs_trafilatura::page_type::classify_url(url).as_str())
    }
    #[cfg(not(feature = "rs-trafilatura"))]
    {
        let _ = url;
        PageType::Unknown
    }
}

/// Error details are third-party text; only this much reaches a log line.
#[allow(dead_code)]
const MAX_DETAIL: usize = 160;

#[allow(dead_code)]
fn detail(e: impl std::fmt::Display) -> String {
    let s = e.to_string();
    let end = crate::floor_char_boundary(&s, MAX_DETAIL);
    s[..end].to_string()
}

/// `rs-trafilatura`: classifies the page (7 WCXB types), extracts with a per-type profile and
/// predicts its own extraction quality, used as the confidence.
#[derive(Clone, Default)]
pub struct RsTrafilatura {
    #[cfg(feature = "rs-trafilatura")]
    pub options: rs_trafilatura::Options,
}

impl MainContent for RsTrafilatura {
    fn candidate(&self) -> Candidate {
        Candidate::RsTrafilatura
    }

    #[cfg(feature = "rs-trafilatura")]
    fn extract(&self, page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        let t = Instant::now();
        let mut options = self.options.clone();
        options.url = page.url.map(str::to_string);
        let html = lighten(page.html);
        let r = rs_trafilatura::extract_with_options(&html, &options)
            .map_err(|e| MainContentError::Failed { candidate: Candidate::RsTrafilatura, detail: detail(e) })?;
        Ok(Extracted {
            candidate: Candidate::RsTrafilatura,
            text: r.content_text,
            title: r.metadata.title.filter(|t| !t.trim().is_empty()),
            page_type: r.metadata.page_type.as_deref().map_or(PageType::Unknown, PageType::parse),
            confidence: Some(r.extraction_quality.clamp(0.0, 1.0) as f32),
            elapsed: t.elapsed(),
        })
    }

    #[cfg(not(feature = "rs-trafilatura"))]
    fn extract(&self, _page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        Err(MainContentError::Unavailable(Candidate::RsTrafilatura))
    }
}

/// `rust-trafilatura`: a native port of go-trafilatura, the fast candidate. It reports neither a
/// confidence nor a WCXB page type.
#[derive(Clone)]
pub struct RustTrafilatura {
    #[cfg(feature = "rust-trafilatura")]
    pub options: rust_trafilatura::Options,
}

impl Default for RustTrafilatura {
    fn default() -> Self {
        Self {
            #[cfg(feature = "rust-trafilatura")]
            options: rust_trafilatura::Options {
                // Dates come from channel metadata; the HTML date search is the slow part.
                html_date_mode: rust_trafilatura::HtmlDateMode::Disabled,
                ..rust_trafilatura::Options::default()
            },
        }
    }
}

impl MainContent for RustTrafilatura {
    fn candidate(&self) -> Candidate {
        Candidate::RustTrafilatura
    }

    #[cfg(feature = "rust-trafilatura")]
    fn extract(&self, page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        let t = Instant::now();
        let mut options = self.options.clone();
        options.original_url = page.url.and_then(rust_trafilatura::Url::request);
        // Already decoded: parse the string rather than letting the crate re-sniff the bytes.
        let html = lighten(page.html);
        let doc = rust_trafilatura::parse_html_with_scripting(&html, false);
        let r = rust_trafilatura::extract_document(&doc, &options)
            .map_err(|e| MainContentError::Failed { candidate: Candidate::RustTrafilatura, detail: detail(e) })?;
        let title = r.metadata.title.trim();
        Ok(Extracted {
            candidate: Candidate::RustTrafilatura,
            text: r.content_text,
            title: (!title.is_empty()).then(|| title.to_string()),
            page_type: PageType::Unknown,
            confidence: None,
            elapsed: t.elapsed(),
        })
    }

    #[cfg(not(feature = "rust-trafilatura"))]
    fn extract(&self, _page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        Err(MainContentError::Unavailable(Candidate::RustTrafilatura))
    }
}
