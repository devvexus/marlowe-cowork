//! Maps a candidate's plain text back onto html.rs blocks, and renders the kept blocks as
//! markdown with inline links, headings, tables and code.
//!
//! Matching is by word trigrams, so it is linear in the page and tolerant of the small edits
//! candidates make (a dropped "Share this", re-joined lines). Short blocks (headings, table cells)
//! match as a whole line of the chosen text, or by sitting between two kept blocks.

use std::collections::HashSet;

use super::{BlockKind, BlockSpan};
use crate::Link;

/// A long block is kept when at least this share of its word trigrams occur in the chosen text.
const KEEP_SHARE: f32 = 0.5;

/// FNV-1a hashes of the lowercased alphanumeric words of `s`, without allocating per word.
fn words(s: &str) -> Vec<u64> {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = Vec::with_capacity(s.len() / 6);
    let mut h = OFFSET;
    let mut in_word = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            in_word = true;
            for l in c.to_lowercase() {
                let mut buf = [0u8; 4];
                for b in l.encode_utf8(&mut buf).bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
            }
        } else if in_word {
            out.push(h);
            h = OFFSET;
            in_word = false;
        }
    }
    if in_word {
        out.push(h);
    }
    out
}

/// Order-sensitive combination of word hashes.
fn hash_words(ws: &[u64]) -> u64 {
    ws.iter().fold(0x9e37_79b9_7f4a_7c15u64, |acc, w| (acc.rotate_left(23) ^ w).wrapping_mul(0xff51_afd7_ed55_8ccd))
}

struct Hay {
    trigrams: HashSet<u64>,
    lines: HashSet<u64>,
    unigrams: HashSet<u64>,
}

impl Hay {
    fn new(chosen: &str) -> Self {
        let all = words(chosen);
        let trigrams = all.windows(3).map(hash_words).collect();
        let unigrams = all.iter().copied().collect();
        let lines = chosen.lines().map(words).filter(|w| !w.is_empty()).map(|w| hash_words(&w)).collect();
        Self { trigrams, lines, unigrams }
    }

    /// Share of `ws`'s trigrams present.
    fn share(&self, ws: &[u64]) -> f32 {
        let n = ws.len().saturating_sub(2);
        if n == 0 {
            return 0.0;
        }
        let hit = ws.windows(3).filter(|t| self.trigrams.contains(&hash_words(t))).count();
        hit as f32 / n as f32
    }

    fn all_words_present(&self, ws: &[u64]) -> bool {
        !ws.is_empty() && ws.iter().all(|w| self.unigrams.contains(w))
    }
}

/// The text a block contributes to matching: a table row is matched as a unit.
fn units(blocks: &[BlockSpan]) -> Vec<(Vec<usize>, Vec<u64>)> {
    let mut out: Vec<(Vec<usize>, Vec<u64>)> = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        if let BlockKind::TableCell { row, .. } = blocks[i].kind {
            let mut idx = Vec::new();
            let mut ws = Vec::new();
            while i < blocks.len() && matches!(blocks[i].kind, BlockKind::TableCell { row: r, .. } if r == row) {
                idx.push(i);
                ws.extend(words(&blocks[i].text));
                i += 1;
            }
            out.push((idx, ws));
        } else {
            out.push((vec![i], words(&blocks[i].text)));
            i += 1;
        }
    }
    out
}

/// Indices of `blocks` whose text appears in `chosen`, in document order: the chosen content
/// mapped back onto structure-bearing blocks.
pub fn align(chosen: &str, blocks: &[BlockSpan]) -> Vec<usize> {
    let hay = Hay::new(chosen);
    let units = units(blocks);
    // Pass 1: long units by trigram share, short units by whole-line equality.
    let mut kept: Vec<Option<bool>> = units
        .iter()
        .map(|(_, ws)| {
            if ws.is_empty() {
                Some(false)
            } else if ws.len() >= 3 {
                Some(hay.share(ws) >= KEEP_SHARE || hay.lines.contains(&hash_words(ws)))
            } else if hay.lines.contains(&hash_words(ws)) {
                Some(true)
            } else {
                None
            }
        })
        .collect();
    // Pass 2: an undecided short unit between two kept units stays when its words occur at all.
    for i in 0..units.len() {
        if kept[i].is_some() {
            continue;
        }
        let before = kept[..i].iter().rev().find_map(|k| *k).unwrap_or(false);
        let after = kept[i + 1..].iter().find_map(|k| *k).unwrap_or(false);
        kept[i] = Some(before && after && hay.all_words_present(&units[i].1));
    }
    units.iter().zip(kept).filter(|(_, k)| *k == Some(true)).flat_map(|((idx, _), _)| idx.iter().copied()).collect()
}

/// Share of the chosen text's words that the kept blocks account for, in `0..=1`.
pub fn coverage(chosen: &str, blocks: &[BlockSpan], keep: &[usize]) -> f32 {
    let total = words(chosen).len();
    if total == 0 {
        return 1.0;
    }
    let kept: usize = keep.iter().filter_map(|&i| blocks.get(i)).map(|b| words(&b.text).len()).sum();
    (kept as f32 / total as f32).min(1.0)
}

/// Wraps each link's anchor text in `text` as `[anchor](url)`, left to right.
fn inline_links(text: &str, link_ids: &[usize], links: &[Link]) -> String {
    let mut out = String::with_capacity(text.len() + link_ids.len() * 40);
    let mut rest = text;
    for &l in link_ids {
        let Some(link) = links.get(l) else { continue };
        // Block text has its whitespace collapsed; an anchor may still carry newlines or runs.
        let collapsed = link.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let anchor = collapsed.as_str();
        if anchor.is_empty() || link.url.is_empty() {
            continue;
        }
        let Some(at) = rest.find(anchor) else { continue };
        out.push_str(&rest[..at]);
        out.push('[');
        out.push_str(&anchor.replace('[', "\\[").replace(']', "\\]"));
        out.push_str("](");
        out.push_str(&link.url.replace(' ', "%20").replace('(', "%28").replace(')', "%29"));
        out.push(')');
        rest = &rest[at + anchor.len()..];
    }
    out.push_str(rest);
    out
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// Renders the kept blocks as markdown. Links are inline; consecutive table cells of one row are
/// one table row, and a table's first row is its header.
pub fn to_markdown(blocks: &[BlockSpan], keep: &[usize], links: &[Link]) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut prev_list = false;
    let mut prev_row: Option<u32> = None;
    while i < keep.len() {
        let Some(b) = blocks.get(keep[i]) else {
            i += 1;
            continue;
        };
        if let BlockKind::TableCell { row, .. } = b.kind {
            let mut cells = Vec::new();
            while i < keep.len() {
                match blocks.get(keep[i]) {
                    Some(c) if matches!(c.kind, BlockKind::TableCell { row: r, .. } if r == row) => {
                        cells.push(cell(&inline_links(c.text.trim(), &c.links, links)));
                        i += 1;
                    }
                    _ => break,
                }
            }
            if prev_row.is_none() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
                out.push_str(&format!("|{}\n", " --- |".repeat(cells.len())));
            } else {
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
            prev_row = Some(row);
            prev_list = false;
            continue;
        }
        if prev_row.take().is_some() {
            out.push('\n');
        }
        let body = b.text.trim();
        let is_list = b.kind == BlockKind::ListItem;
        if !out.is_empty() && !(is_list && prev_list) {
            out.push('\n');
        }
        match b.kind {
            BlockKind::Heading(n) => {
                out.push_str(&"#".repeat(n.clamp(1, 6) as usize));
                out.push(' ');
                out.push_str(&inline_links(body, &b.links, links));
            }
            BlockKind::ListItem => {
                out.push_str("- ");
                out.push_str(&inline_links(body, &b.links, links));
            }
            BlockKind::Code => {
                out.push_str("```\n");
                out.push_str(body);
                out.push_str("\n```");
            }
            BlockKind::Quote => {
                out.push_str("> ");
                out.push_str(&inline_links(body, &b.links, links));
            }
            BlockKind::Paragraph | BlockKind::TableCell { .. } => out.push_str(&inline_links(body, &b.links, links)),
        }
        out.push('\n');
        prev_list = is_list;
        i += 1;
    }
    let out = out.trim_end();
    let end = crate::floor_char_boundary(out, crate::MAX_TEXT_CHARS);
    out[..end].to_string()
}
