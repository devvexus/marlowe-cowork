//! Standing words checked in code. A sentence that grades its own evidence ("two independent
//! sources show", "established", "corroborated") is held to the harness's count of independent
//! roots over the claims it cites; the support check cannot see roots, so the writer's grading
//! was unchecked before. Found live: one arXiv paper (its abs and html pages) was written up as
//! "reported by two independent sources" in the answer and four sentences of a section.
//!
//! An overclaim is rewritten to what the count supports ("one source reports ..."), and the
//! sentence is listed in the report's checks.

use marlowe_research::{ClaimId, DerivationGraph, SourceId, TrustBucket};

use super::parse::{markers, sentences};

/// Number words a sentence may count sources with.
const COUNTS: [(&str, usize); 9] = [
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("several", 2),
    ("multiple", 2),
    ("many", 2),
    ("both", 2),
    ("various", 2),
];
/// Nouns a count of sources is made with.
const NOUNS: [&str; 10] =
    ["sources", "studies", "papers", "works", "reports", "groups", "teams", "articles", "preprints", "evaluations"];
/// Verbs whose plural form follows a plural count ("two sources report").
const VERBS: [&str; 16] = [
    "report", "show", "find", "state", "confirm", "agree", "suggest", "indicate", "describe", "note", "observe",
    "demonstrate", "support", "claim", "document", "measure",
];

/// What the evidence behind one sentence supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backing {
    /// Independent fetched roots over the sources of the cited claims.
    pub roots: usize,
}

/// The independent roots behind `claims`: their sources, memory left out, collapsed over the
/// derivation graph (unresolved pairs and page identities count once).
pub fn backing(
    claims: &[ClaimId],
    source_of: &dyn Fn(ClaimId) -> Option<SourceId>,
    bucket: &dyn Fn(SourceId) -> TrustBucket,
    graph: &DerivationGraph,
) -> Backing {
    let mut sources: Vec<SourceId> =
        claims.iter().filter_map(|c| source_of(*c)).filter(|s| bucket(*s) == TrustBucket::Untrusted).collect();
    sources.sort();
    sources.dedup();
    Backing { roots: graph.independent_roots(&sources) }
}

/// One word of a sentence with its byte span.
fn words(s: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        let part = c.is_alphanumeric() || c == '-';
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(a)) => {
                out.push((a, i, s[a..i].to_ascii_lowercase()));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(a) = start {
        out.push((a, s.len(), s[a..].to_ascii_lowercase()));
    }
    out
}

/// The sentence with its grading brought down to `b`, or `None` when it claims no more than
/// the count supports.
pub fn correct(sentence: &str, b: Backing) -> Option<String> {
    let w = words(sentence);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < w.len() {
        let (a, e, word) = &w[i];
        // "<count> [independent|separate|different] <noun> [verb]"
        if let Some(&(_, n)) = COUNTS.iter().find(|(c, _)| c == word) {
            let mut j = i + 1;
            if w.get(j).is_some_and(|x| matches!(x.2.as_str(), "independent" | "separate" | "different" | "distinct")) {
                j += 1;
            }
            if let Some((_, ne, noun)) = w.get(j).filter(|x| NOUNS.contains(&x.2.as_str())) {
                if b.roots < n {
                    let replacement = if b.roots <= 1 { "one source".to_string() } else { format!("{} independent sources", b.roots) };
                    let replacement = keep_case(&sentence[*a..*e], &replacement);
                    let _ = noun;
                    edits.push((*a, *ne, replacement));
                    // "two sources report" -> "one source reports".
                    if b.roots <= 1 {
                        if let Some((va, ve, verb)) = w.get(j + 1) {
                            if VERBS.contains(&verb.as_str()) {
                                edits.push((*va, *ve, format!("{}s", &sentence[*va..*ve])));
                            } else if verb == "are" {
                                edits.push((*va, *ve, "is".into()));
                            } else if verb == "have" {
                                edits.push((*va, *ve, "has".into()));
                            }
                        }
                    }
                }
                i = j + 1;
                continue;
            }
        }
        if b.roots < 2 {
            match word.as_str() {
                "established" | "corroborated" => {
                    // "an established" -> "a reported".
                    if let Some((pa, pe, prev)) = i.checked_sub(1).and_then(|k| w.get(k)) {
                        if prev == "an" {
                            edits.push((*pa, *pe, keep_case(&sentence[*pa..*pe], "a")));
                        }
                    }
                    edits.push((*a, *e, keep_case(&sentence[*a..*e], "reported")));
                }
                "independently" => {
                    // Drop it with the space after it.
                    let end = sentence[*e..].find(|c: char| !c.is_whitespace()).map_or(*e, |k| e + k);
                    edits.push((*a, end, String::new()));
                }
                _ => {}
            }
        }
        i += 1;
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(sentence.len());
    let mut last = 0;
    for (a, e, r) in edits {
        if a < last {
            continue;
        }
        out.push_str(&sentence[last..a]);
        out.push_str(&r);
        last = e;
    }
    out.push_str(&sentence[last..]);
    Some(out)
}

/// `to` with the first letter's case of `from`.
fn keep_case(from: &str, to: &str) -> String {
    match (from.chars().next(), to.chars().next()) {
        (Some(f), Some(t)) if f.is_uppercase() => t.to_uppercase().chain(to.chars().skip(1)).collect(),
        _ => to.to_string(),
    }
}

/// Every cited sentence of `text` corrected against its backing; the corrections, as (before,
/// after), go to `changed`.
pub fn correct_text(text: &str, backing_of: &dyn Fn(&[ClaimId]) -> Backing, changed: &mut Vec<(String, String)>) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut out = String::new();
        let mut last = 0;
        for (a, e) in sentences(line) {
            let s = &line[a..e];
            let claims: Vec<ClaimId> = markers(s).into_iter().flat_map(|m| m.ids).map(ClaimId).collect();
            out.push_str(&line[last..a]);
            last = e;
            match (!claims.is_empty()).then(|| correct(s, backing_of(&claims))).flatten() {
                Some(fixed) => {
                    changed.push((s.to_string(), fixed.clone()));
                    out.push_str(&fixed);
                }
                None => out.push_str(s),
            }
        }
        out.push_str(&line[last..]);
        lines.push(out);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: Backing = Backing { roots: 1 };
    const TWO: Backing = Backing { roots: 2 };

    #[test]
    fn a_single_root_described_as_two_independent_sources_is_brought_down() {
        let s = "NOT is reported by two independent sources to transplant layers [C37, C38].";
        assert_eq!(correct(s, ONE).as_deref(), Some("NOT is reported by one source to transplant layers [C37, C38]."));
        let s = "Two independent sources report that code is public [C40].";
        assert_eq!(correct(s, ONE).as_deref(), Some("One source reports that code is public [C40]."));
        let s = "Several studies are consistent on this [C1].";
        assert_eq!(correct(s, ONE).as_deref(), Some("One source is consistent on this [C1]."));
        assert_eq!(correct("Three papers agree [C1, C2].", TWO).as_deref(), Some("2 independent sources agree [C1, C2]."));
        assert_eq!(correct("Two independent sources agree [C1, C2].", TWO), None, "the count holds");
    }

    #[test]
    fn established_and_independently_need_two_roots() {
        let s = "This is an established result, independently confirmed [C3].";
        assert_eq!(correct(s, ONE).as_deref(), Some("This is a reported result, confirmed [C3]."));
        assert_eq!(correct(s, TWO), None);
        assert_eq!(correct("One source reports 2.8x faster training [C9].", ONE), None, "honest grading is kept");
    }

    #[test]
    fn whole_texts_are_corrected_sentence_by_sentence() {
        let text = "Two sources show X [C1]. Y holds [C2].\nAn established Z [C1, C2].";
        let mut changed = Vec::new();
        let roots = |c: &[ClaimId]| Backing { roots: c.len() };
        let out = correct_text(text, &roots, &mut changed);
        assert_eq!(out, "One source shows X [C1]. Y holds [C2].\nAn established Z [C1, C2].");
        assert_eq!(changed.len(), 1);
    }
}
