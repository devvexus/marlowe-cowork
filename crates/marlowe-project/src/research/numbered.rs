//! Numbered links: what a model reads, so it never writes a url.
//!
//! The extractor renders links as markdown, `[anchor](url)`. A page goes to a model as
//! `anchor [L12]` instead, with `12` the row of the page's link table (1-based, in order of first
//! appearance, deduplicated by canonical url, at most 400 rows). The model points at numbers; code
//! looks the exact url up. The text that is stored is exactly the text the model saw, so a quote
//! can be located byte for byte later.
//!
//! This is a rewrite of the extractor's own markdown by link-table index rather than a second
//! renderer: it needs nothing from the extractor and drops nothing it kept.

use marlowe_extract::Link;
use marlowe_research::{canonical_url, DocRef, LinkRow, LinkTable};
use serde::{Deserialize, Serialize};

/// A page ready for a model: numbered text and its link table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Numbered {
    /// The text with `[L<n>]` after each linked anchor.
    pub text: String,
    /// The link table: `rows[n - 1]` is link `n`.
    pub links: Vec<LinkRow>,
}

impl Numbered {
    /// The exact url of link `n`.
    pub fn url(&self, n: u32) -> Option<&str> {
        n.checked_sub(1).and_then(|i| self.links.get(i as usize)).map(|r| r.url.as_str())
    }
}

/// Undoes the extractor's escaping of an anchor.
fn unescape_anchor(s: &str) -> String {
    s.replace("\\[", "[").replace("\\]", "]")
}

/// Undoes the extractor's percent-escaping of the characters that would end a markdown link.
fn unescape_url(s: &str) -> String {
    s.replace("%28", "(").replace("%29", ")").replace("%20", " ")
}

/// One markdown link found in the text: the byte span of the whole `[..](..)`, the anchor, the url
/// and whether it was an image (`![alt](src)`).
struct Found {
    start: usize,
    end: usize,
    anchor: String,
    url: String,
    image: bool,
}

fn scan(md: &str) -> Vec<Found> {
    let b = md.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] != b'[' {
            i += 1;
            continue;
        }
        // The anchor ends at the first unescaped `]`.
        let mut j = i + 1;
        let mut close = None;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b']' => {
                    close = Some(j);
                    break;
                }
                b'\n' if j > i + 400 => break,
                _ => j += 1,
            }
        }
        let Some(close) = close else {
            i += 1;
            continue;
        };
        if b.get(close + 1) != Some(&b'(') {
            i += 1;
            continue;
        }
        let url_start = close + 2;
        let Some(rel) = md[url_start..].find(')') else {
            i += 1;
            continue;
        };
        let url = &md[url_start..url_start + rel];
        // `(url "title")`: keep the url only.
        let url = url.split_whitespace().next().unwrap_or("");
        if url.is_empty() {
            i += 1;
            continue;
        }
        let image = i > 0 && b[i - 1] == b'!';
        let start = if image { i - 1 } else { i };
        out.push(Found { start, end: url_start + rel + 1, anchor: unescape_anchor(&md[i + 1..close]), url: unescape_url(url), image });
        i = url_start + rel + 1;
    }
    out
}

/// Rewrites `md` with numbered links and builds the page's link table.
///
/// An image keeps its alt text and loses its source. A link that the table does not keep (not
/// http or https, the page itself) keeps its anchor and gets no number.
pub fn number_links(md: &str, page_url: &str) -> Numbered {
    let found = scan(md);
    let ordered: Vec<Link> = found.iter().filter(|f| !f.image).map(|f| Link { url: f.url.clone(), text: f.anchor.clone() }).collect();
    let table = LinkTable::from_links(DocRef::of(md.as_bytes()), page_url, &ordered);
    let number_of = |url: &str| -> Option<u32> {
        let want = canonical_url(url.split('#').next().unwrap_or(""))?;
        table.rows().iter().find(|r| canonical_url(&r.url).as_deref() == Some(want.as_str())).map(|r| r.index)
    };
    let mut out = String::with_capacity(md.len());
    let mut at = 0;
    for f in &found {
        out.push_str(&md[at..f.start]);
        out.push_str(&f.anchor);
        if !f.image {
            if let Some(n) = number_of(&f.url) {
                out.push_str(&format!(" [L{n}]"));
            }
        }
        at = f.end;
    }
    out.push_str(&md[at..]);
    Numbered { text: out, links: table.rows().to_vec() }
}

/// `text` without its ` [L12]` markers, and for each byte of the result the byte it came from.
fn strip_markers(text: &str) -> (String, Vec<usize>) {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
    let mut i = 0;
    while i < b.len() {
        if b[i] == b' ' && b.get(i + 1) == Some(&b'[') && b.get(i + 2) == Some(&b'L') {
            let mut j = i + 3;
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            if j > i + 3 && b.get(j) == Some(&b']') {
                i = j + 1;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        for k in 0..c.len_utf8() {
            map.push(i + k);
        }
        out.push(c);
        i += c.len_utf8();
    }
    map.push(text.len());
    (out, map)
}

/// Finds `quote` in the stored page text. A model that copied a sentence but left out the `[L12]`
/// marker after a link still finds it: the quote is matched with the markers set aside and the
/// returned quote is the page's own bytes, markers included.
pub fn locate_quote(text: &str, quote: &str) -> Option<marlowe_research::Quote> {
    let doc = DocRef::of(text.as_bytes());
    if let Some(q) = marlowe_research::text::locate(&doc, text, quote) {
        return Some(q);
    }
    let (plain, map) = strip_markers(text);
    if plain.len() == text.len() {
        return None;
    }
    let found = marlowe_research::text::locate(&DocRef::of(plain.as_bytes()), &plain, quote)?;
    let start = *map.get(found.locator.start)?;
    let last = found.locator.end.checked_sub(1)?;
    let last_start = *map.get(last)?;
    let end = last_start + text[last_start..].chars().next()?.len_utf8();
    let q = marlowe_research::Quote { text: text.get(start..end)?.to_string(), locator: marlowe_research::Locator { doc, start, end, section: None, page: None } };
    q.matches(text).then_some(q)
}

/// At most `cap` characters of `text`, kept in their original order: when the page is longer, the
/// paragraphs that share the most words with `focus` are kept first, then the opening ones, and a
/// line says how much was left out. Quotes are still located in the full stored text.
pub fn fit(text: &str, focus: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let words: Vec<String> = focus
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect();
    let paras: Vec<&str> = text.split("\n\n").collect();
    let score = |p: &str| -> usize {
        let l = p.to_lowercase();
        words.iter().filter(|w| l.contains(w.as_str())).count()
    };
    let mut order: Vec<usize> = (0..paras.len()).collect();
    // Opening paragraphs count for something (titles, abstracts), the best matches for more.
    order.sort_by_key(|&i| (std::cmp::Reverse(score(paras[i]) * 4 + usize::from(i < 3) * 3), i));
    let mut keep = vec![false; paras.len()];
    let mut used = 0;
    for i in order {
        let n = paras[i].chars().count() + 2;
        if used + n > cap && used > 0 {
            continue;
        }
        keep[i] = true;
        used += n;
    }
    let mut out = String::new();
    let mut skipped = 0;
    for (i, p) in paras.iter().enumerate() {
        if keep[i] {
            if skipped > 0 {
                out.push_str(&format!("[... {skipped} paragraphs left out ...]\n\n"));
                skipped = 0;
            }
            // A single paragraph longer than the cap is cut on a char boundary.
            let p = match p.char_indices().nth(cap) {
                Some((k, _)) => &p[..k],
                None => p,
            };
            out.push_str(p);
            out.push_str("\n\n");
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push_str(&format!("[... {skipped} paragraphs left out ...]\n"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "# Kimi K2.6 released\n\nMoonshot AI [announced Kimi K2.6](https://moonshot.example/blog/k2-6) on 2026-10-20, \
        following [K2.5](https://moonshot.example/blog/k2-5#top) and [the paper](https://arxiv.org/abs/2507.20534v2). \
        See [K2.5](https://moonshot.example/blog/k2-5) again, and [this page](https://news.example/self) and \
        [mail](mailto:a@b.c) and ![chart](https://img.example/c.png) and [odd \\[bracket\\]](https://x.example/a%28b%29).";

    #[test]
    fn links_are_numbered_by_first_appearance_and_repeats_share_a_number() {
        let n = number_links(PAGE, "https://news.example/self");
        assert!(n.text.contains("announced Kimi K2.6 [L1] on"), "{}", n.text);
        assert!(n.text.contains("K2.5 [L2] and the paper [L3]"), "{}", n.text);
        assert!(n.text.contains("See K2.5 [L2] again"), "the same canonical url keeps its number");
        assert_eq!(n.url(1), Some("https://moonshot.example/blog/k2-6"));
        assert_eq!(n.url(2), Some("https://moonshot.example/blog/k2-5"));
        assert_eq!(n.url(3), Some("https://arxiv.org/abs/2507.20534v2"));
        assert_eq!(n.url(99), None);
        assert_eq!(n.url(0), None);
    }

    #[test]
    fn the_page_itself_mailto_and_images_get_no_number_and_no_url_survives_in_the_text() {
        let n = number_links(PAGE, "https://news.example/self");
        assert!(n.text.contains("and this page and mail and chart and"), "{}", n.text);
        assert!(!n.text.contains("http"), "a model reading this text sees no url: {}", n.text);
        assert!(n.text.contains("odd [bracket] [L4]"), "{}", n.text);
        assert_eq!(n.url(4), Some("https://x.example/a(b)"), "escaped parentheses are restored");
    }

    #[test]
    fn text_without_links_is_unchanged_and_the_numbering_is_deterministic() {
        let plain = "Just words.\n\nMore [not a link] words (and parens).";
        let n = number_links(plain, "https://a.example/");
        assert_eq!(n.text, plain);
        assert!(n.links.is_empty());
        assert_eq!(number_links(PAGE, "https://news.example/self"), number_links(PAGE, "https://news.example/self"));
    }

    #[test]
    fn a_quote_is_found_with_or_without_the_link_markers_and_stays_the_pages_own_bytes() {
        let n = number_links("# T

Moonshot AI [announced Kimi K2.6](https://moonshot.example/blog/k2-6) on 2026-10-20, with a 256k context window.", "https://news.example/x");
        let with = locate_quote(&n.text, "announced Kimi K2.6 [L1] on 2026-10-20").unwrap();
        let without = locate_quote(&n.text, "announced Kimi K2.6 on 2026-10-20, with a 256k context window.").unwrap();
        assert!(with.matches(&n.text) && without.matches(&n.text));
        assert!(without.text.contains("[L1]"), "{}", without.text);
        assert!(without.text.ends_with("window."));
        assert!(locate_quote(&n.text, "announced Kimi K9 on 2026-10-20").is_none());
    }

    #[test]
    fn a_link_title_is_dropped_with_the_url() {
        let n = number_links("a [b](https://x.example/p \"the title\") c", "https://a.example/");
        assert_eq!(n.text, "a b [L1] c");
        assert_eq!(n.url(1), Some("https://x.example/p"));
    }

    #[test]
    fn fit_keeps_the_best_paragraphs_in_their_order_and_says_what_it_left_out() {
        let mut paras: Vec<String> = (0..40).map(|i| format!("Filler paragraph number {i} about nothing in particular at all, padded {}", "x".repeat(200))).collect();
        paras[30] = "The Kimi K2.6 release has 1T parameters and a 256k context window.".into();
        let text = paras.join("\n\n");
        let out = fit(&text, "Kimi K2.6 release parameters context", 1500);
        assert!(out.chars().count() <= 1700, "{}", out.chars().count());
        assert!(out.contains("The Kimi K2.6 release has 1T parameters"), "the matching paragraph survives");
        assert!(out.starts_with("Filler paragraph number 0"), "the opening is kept");
        assert!(out.contains("paragraphs left out"));
        assert!(out.find("number 1 ").unwrap() < out.find("The Kimi").unwrap(), "order is the page's order");
        assert_eq!(fit("short", "x", 100), "short");
    }
}
