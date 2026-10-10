//! Navigation by SELECTION (ADR-043): the harness builds a link table from a fetched page's
//! extracted markup, the model picks an index, and the harness supplies the URL bytes. A
//! model-composed URL is never fetched. The table is built here, never by the reader that
//! summarised the page.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::source::canonical_url;

/// Rows kept per page.
pub const MAX_LINKS: usize = 400;
/// Characters of anchor text kept per row.
pub const MAX_ANCHOR_CHARS: usize = 120;

/// A fetched document's content address: blake3 of its bytes, hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocRef(pub String);

impl DocRef {
    pub fn of(bytes: &[u8]) -> Self {
        DocRef(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 hex chars, for the model-facing table header.
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }
}

/// One selectable link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRow {
    /// 1-based, what the model passes back.
    pub index: u32,
    /// The URL as extracted and resolved by the harness (fragment dropped).
    pub url: String,
    /// Anchor text: page content, capped and stripped of control characters.
    pub text: String,
    pub host: String,
}

/// The selectable links of one fetched page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTable {
    pub doc: DocRef,
    pub page_url: String,
    rows: Vec<LinkRow>,
    /// Links dropped by the cap, duplicates and non-http(s) schemes.
    pub dropped: usize,
}

/// Anchor text as the model may see it.
pub fn clean_anchor(raw: &str) -> String {
    let flat: String = raw.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let squashed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    match squashed.char_indices().nth(MAX_ANCHOR_CHARS) {
        Some((i, _)) => format!("{}…", &squashed[..i]),
        None => squashed,
    }
}

impl LinkTable {
    /// Builds the table from extracted links: http(s) only, duplicates (by canonical URL) and
    /// links back to the page itself removed, capped at [`MAX_LINKS`].
    pub fn from_links(doc: DocRef, page_url: &str, links: &[marlowe_extract::Link]) -> Self {
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(c) = canonical_url(page_url) {
            seen.insert(c);
        }
        let mut rows = Vec::new();
        let mut dropped = 0;
        for l in links {
            let url = l.url.split('#').next().unwrap_or("").trim();
            let Some(canon) = canonical_url(url) else {
                dropped += 1;
                continue;
            };
            if rows.len() >= MAX_LINKS || !seen.insert(canon) {
                dropped += 1;
                continue;
            }
            let host =
                url.split("://").nth(1).and_then(|r| r.split(['/', '?']).next()).unwrap_or("").to_ascii_lowercase();
            rows.push(LinkRow {
                index: rows.len() as u32 + 1,
                url: url.to_string(),
                text: clean_anchor(&l.text),
                host,
            });
        }
        Self { doc, page_url: page_url.to_string(), rows, dropped }
    }

    pub fn rows(&self) -> &[LinkRow] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, index: u32) -> Option<&LinkRow> {
        index.checked_sub(1).and_then(|i| self.rows.get(i as usize))
    }

    /// The model-facing listing: `[7] host — anchor`, at most `max` rows.
    pub fn render(&self, max: usize) -> String {
        let mut out = format!("links of {} (select by number):", self.doc.short());
        for r in self.rows.iter().take(max) {
            out.push_str(&format!(
                "\n[{}] {} — {}",
                r.index,
                r.host,
                if r.text.is_empty() { "(no text)" } else { &r.text }
            ));
        }
        if self.rows.len() > max {
            out.push_str(&format!("\n[{} more not shown]", self.rows.len() - max));
        }
        out
    }
}

/// Why a selection did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("no link table for document {0}")]
    UnknownDoc(String),
    #[error("link {index} does not exist; the table has {len}")]
    NoSuchLink { index: u32, len: usize },
}

/// Every link table a run has built; resolves `(doc, index)` to the harness's URL.
#[derive(Debug, Clone, Default)]
pub struct Navigator {
    tables: HashMap<DocRef, LinkTable>,
}

impl Navigator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, table: LinkTable) {
        self.tables.insert(table.doc.clone(), table);
    }

    pub fn table(&self, doc: &DocRef) -> Option<&LinkTable> {
        self.tables.get(doc)
    }

    /// The only way a follow-up URL is produced: an index into a harness-built table.
    pub fn select(&self, doc: &DocRef, index: u32) -> Result<&LinkRow, SelectError> {
        let t = self.tables.get(doc).ok_or_else(|| SelectError::UnknownDoc(doc.short().to_string()))?;
        t.get(index).ok_or(SelectError::NoSuchLink { index, len: t.len() })
    }

    /// Resolves a short (prefix) doc ref the model saw in a rendered table, when unambiguous.
    pub fn resolve_short(&self, short: &str) -> Option<&DocRef> {
        let mut hits = self.tables.keys().filter(|d| !short.is_empty() && d.as_str().starts_with(short));
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_extract::Link;

    fn link(url: &str, text: &str) -> Link {
        Link { url: url.into(), text: text.into() }
    }

    #[test]
    fn tables_keep_http_links_once_and_select_by_index() {
        let doc = DocRef::of(b"page");
        let links = vec![
            link("https://arxiv.org/abs/2106.09685#sec2", "LoRA\u{1b}[31m paper"),
            link("https://arxiv.org/abs/2106.09685?utm_source=x", "dup"),
            link("javascript:alert(1)", "x"),
            link("mailto:a@b.c", "mail"),
            link("https://example.com/page", "self"),
            link("https://github.com/microsoft/LoRA", &"long ".repeat(100)),
        ];
        let t = LinkTable::from_links(doc.clone(), "https://example.com/page/", &links);
        assert_eq!(t.len(), 2);
        assert_eq!(t.dropped, 4);
        assert_eq!(t.get(1).unwrap().url, "https://arxiv.org/abs/2106.09685");
        assert_eq!(t.get(1).unwrap().text, "LoRA [31m paper");
        assert_eq!(t.get(2).unwrap().host, "github.com");
        assert!(t.get(2).unwrap().text.chars().count() <= MAX_ANCHOR_CHARS + 1);
        assert!(t.get(0).is_none() && t.get(3).is_none());

        let mut nav = Navigator::new();
        nav.insert(t);
        assert_eq!(nav.select(&doc, 2).unwrap().url, "https://github.com/microsoft/LoRA");
        assert_eq!(nav.select(&doc, 9), Err(SelectError::NoSuchLink { index: 9, len: 2 }));
        assert!(matches!(nav.select(&DocRef::of(b"other"), 1), Err(SelectError::UnknownDoc(_))));
        assert_eq!(nav.resolve_short(doc.short()), Some(&doc));
        let r = nav.table(&doc).unwrap().render(1);
        assert!(r.contains("[1] arxiv.org — LoRA") && r.contains("[1 more not shown]"), "{r}");
    }

    #[test]
    fn the_cap_holds() {
        let links: Vec<Link> = (0..MAX_LINKS + 50).map(|i| link(&format!("https://e.org/{i}"), "x")).collect();
        let t = LinkTable::from_links(DocRef::of(b"big"), "https://e.org/", &links);
        assert_eq!((t.len(), t.dropped), (MAX_LINKS, 50));
    }
}
