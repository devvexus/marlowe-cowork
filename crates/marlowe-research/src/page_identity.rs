//! What a fetched page says it is: the Highwire / Dublin Core / PRISM `<meta>` tags scholarly
//! hosts publish (`citation_doi`, `citation_arxiv_id`, `citation_title`, `citation_author`, ...),
//! and arXiv's own stamp near the top of a paper's text (`arXiv:2601.13580v1 [cs.LG]`).
//!
//! A page's identifiers are claims (ADR-036 §5). The route built here is a `Web` route with no
//! URL: added to the deduper it pairs the page with the work it names as `Unresolved`, which
//! counts as one root for corroboration and never merges two sources.

use marlowe_proto::ChannelId;

use crate::source::{normalize_arxiv, Identity, SourceRef};

/// Characters of the page text searched for arXiv's stamp.
const HEAD_CHARS: usize = 4_000;
/// Authors kept from meta tags.
const MAX_AUTHORS: usize = 12;

/// The meta names read, lower-cased. Everything else on a page is ignored.
pub fn is_identity_meta(name: &str) -> bool {
    matches!(
        name,
        "citation_doi"
            | "citation_arxiv_id"
            | "citation_title"
            | "citation_author"
            | "citation_publication_date"
            | "citation_date"
            | "citation_online_date"
            | "dc.identifier"
            | "dc.title"
            | "dc.creator"
            | "dc.date"
            | "prism.doi"
    )
}

/// The page's own record of its work, or `None` when it states no identifier and no title with
/// an author. `meta` is `(name, content)` as the page gave it; `text` is its extracted text.
pub fn page_route(meta: &[(String, String)], text: &str) -> Option<SourceRef> {
    let get = |names: &[&str]| -> Option<String> {
        meta.iter()
            .find(|(n, v)| names.iter().any(|w| n.eq_ignore_ascii_case(w)) && !v.trim().is_empty())
            .map(|(_, v)| v.trim().to_string())
    };
    let mut identity = Identity::default();
    for (n, v) in meta {
        let n = n.to_ascii_lowercase();
        match n.as_str() {
            "citation_doi" | "prism.doi" => identity = identity.with_doi(v),
            "dc.identifier" if v.to_ascii_lowercase().contains("10.") => identity = identity.with_doi(v),
            "citation_arxiv_id" => identity = identity.with_arxiv(v),
            _ => {}
        }
    }
    if identity.arxiv_id.is_none() {
        if let Some(id) = arxiv_stamp(text) {
            identity = identity.with_arxiv(&id);
        }
    }
    let title = get(&["citation_title", "dc.title"]).unwrap_or_default();
    let authors: Vec<String> = meta
        .iter()
        .filter(|(n, v)| (n.eq_ignore_ascii_case("citation_author") || n.eq_ignore_ascii_case("dc.creator")) && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_string())
        .take(MAX_AUTHORS)
        .collect();
    if identity.is_empty() && (title.is_empty() || authors.is_empty()) {
        return None;
    }
    let mut r = SourceRef::new(ChannelId::Web, "", &title);
    r.identity = (!identity.is_empty()).then_some(identity);
    r.authors = authors;
    r.date = get(&["citation_publication_date", "citation_date", "citation_online_date", "dc.date"]);
    Some(r)
}

/// arXiv's submission stamp, `arXiv:2601.13580v1 [cs.LG] 20 Jan 2026`, in the first few
/// thousand characters: the paper's own id, not one it cites (a citation carries no `[category]`).
fn arxiv_stamp(text: &str) -> Option<String> {
    let head: String = text.chars().take(HEAD_CHARS).collect();
    let mut rest = head.as_str();
    while let Some(i) = rest.find("arXiv:") {
        let after = &rest[i + 6..];
        let id: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-')).collect();
        let tail = after[id.len()..].trim_start();
        if tail.starts_with('[') {
            if let Some(n) = normalize_arxiv(&id) {
                return Some(n);
            }
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn highwire_tags_give_the_identity_title_and_authors() {
        let meta = m(&[
            ("citation_title", "SparseGPT: Massive Language Models Can Be Accurately Pruned in One-Shot"),
            ("citation_author", "Frantar, Elias"),
            ("citation_author", "Alistarh, Dan"),
            ("citation_publication_date", "2023/07/03"),
            ("citation_arxiv_id", "2301.00774v3"),
            ("description", "ignored"),
        ]);
        let r = page_route(&meta, "").unwrap();
        assert_eq!(r.identity.as_ref().unwrap().arxiv_id.as_deref(), Some("2301.00774"));
        assert_eq!(r.authors, ["Frantar, Elias", "Alistarh, Dan"]);
        assert_eq!(r.year(), Some(2023));
        assert!(r.url.is_empty() && r.channel == ChannelId::Web, "a claim, never an identity merge");
        let doi = page_route(&m(&[("DC.Identifier", "doi:10.18653/v1/2026.acl-long.276")]), "").unwrap();
        assert_eq!(doi.identity.unwrap().doi.as_deref(), Some("10.18653/v1/2026.acl-long.276"));
    }

    #[test]
    fn the_arxiv_stamp_is_the_papers_own_id_and_a_citation_is_not() {
        let paper = "Neural Organ Transplantation\narXiv:2601.13580v1 [cs.LG] 20 Jan 2026\nAbstract ...";
        assert_eq!(page_route(&[], paper).unwrap().identity.unwrap().arxiv_id.as_deref(), Some("2601.13580"));
        let blog = "We build on SparseGPT (arXiv:2301.00774) and Wanda.";
        assert!(page_route(&[], blog).is_none());
        assert!(page_route(&m(&[("citation_title", "A title alone")]), "").is_none(), "a title needs an author");
    }
}
