//! Session episodes, written without a model: the memory engine picks what to keep.
//!
//! An episode is the session's opening request (its title), when and where it ran, the lines
//! that carry most of the session's distinctive vocabulary, and its keywords. Salience is BM25's
//! term weighting: a term's frequency in the session against its document frequency across the
//! whole store, so words every session uses weigh nothing and the session's own subject weighs
//! most. The day is written out in words ("Tuesday 6 October 2026"), so "what were we doing on
//! Tuesday?" finds the episode through ordinary retrieval. No generative model is involved and
//! nothing loads.

use std::collections::BTreeMap;

use chrono::{Local, TimeZone};
use marlowe_memory::cue::lexical::tokenize;

/// One ingested turn of the session, oldest first.
pub(crate) struct Turn<'a> {
    pub user: bool,
    pub at_ms: i64,
    pub text: &'a str,
}

pub(crate) const MAX_USER_LINES: usize = 3;
pub(crate) const MAX_ASSISTANT_LINES: usize = 2;
pub(crate) const MAX_KEYWORDS: usize = 8;
const TITLE_CHARS: usize = 100;
const LINE_CHARS: usize = 220;
/// A sentence shorter than this (in tokens) says too little to keep.
const MIN_LINE_TOKENS: usize = 4;
/// A line scoring under this share of its group's best is left out.
const MIN_LINE_SHARE: f64 = 0.25;

/// Function words, kept out of keywords and salience. Not on the retrieval path: the lexical cue
/// itself has no stop list.
const STOP: &[&str] = &[
    "about", "after", "again", "all", "also", "and", "any", "are", "because", "been", "before", "being", "but", "can",
    "could", "did", "does", "doing", "done", "for", "from", "get", "got", "had", "has", "have", "here", "how", "into",
    "its", "just", "let", "like", "make", "more", "most", "much", "need", "not", "now", "one", "only", "our", "out",
    "over", "please", "should", "some", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "those", "through", "too", "use", "very", "want", "was", "way", "well", "were", "what", "when", "where",
    "which", "while", "who", "why", "will", "with", "would", "yes", "you", "your", "i'm", "it's", "don", "won",
];

fn is_term(t: &str) -> bool {
    t.chars().count() >= 3 && !t.chars().all(|c| c.is_ascii_digit()) && !STOP.contains(&t)
}

/// The episode text, or `None` when there are no turns.
///
/// `docs` is the store's document count and `df` gives each term's document frequency in it
/// (the session's own turns included).
pub(crate) fn compose(turns: &[Turn<'_>], workspace: Option<&str>, docs: usize, df: impl Fn(&[&str]) -> Vec<usize>) -> Option<String> {
    let first = turns.first()?;
    let last = turns.last()?;
    let opening = turns.iter().find(|t| t.user).unwrap_or(first);
    let title = clip(&first_sentence(opening.text), TITLE_CHARS);

    // Session term frequencies over meaningful terms.
    let mut tf: BTreeMap<String, u32> = BTreeMap::new();
    for t in turns {
        for term in tokenize(t.text) {
            if is_term(&term) {
                *tf.entry(term).or_insert(0) += 1;
            }
        }
    }
    let terms: Vec<&str> = tf.keys().map(String::as_str).collect();
    let freqs = df(&terms);
    let n = docs.max(1) as f64;
    let weight: BTreeMap<&str, f64> = terms
        .iter()
        .zip(&freqs)
        .map(|(t, d)| {
            let d = (*d as f64).min(n);
            let idf = (1.0 + (n - d + 0.5) / (d + 0.5)).ln();
            (*t, (1.0 + f64::from(tf[*t]).ln()) * idf)
        })
        .collect();

    // Candidate lines, scored by the weight of the distinct terms they carry.
    let mut lines: Vec<(bool, usize, f64, String)> = Vec::new();
    let mut order = 0usize;
    for t in turns {
        for s in sentences(t.text) {
            order += 1;
            let toks = tokenize(&s);
            if toks.len() < MIN_LINE_TOKENS {
                continue;
            }
            let mut seen: Vec<&str> = Vec::new();
            let mut score = 0.0;
            for tok in &toks {
                if let Some((k, w)) = weight.get_key_value(tok.as_str()) {
                    if !seen.contains(k) {
                        seen.push(k);
                        score += w;
                    }
                }
            }
            let score = score / (toks.len() as f64).sqrt();
            if score > 0.0 {
                lines.push((t.user, order, score, clip(&s, LINE_CHARS)));
            }
        }
    }
    let pick = |user: bool, max: usize| -> Vec<String> {
        let mut c: Vec<&(bool, usize, f64, String)> =
            lines.iter().filter(|l| l.0 == user && !(user && l.3 == title)).collect();
        c.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.1.cmp(&b.1)));
        // A line far below the best says little the others do not.
        let floor = c.first().map_or(0.0, |l| l.2 * MIN_LINE_SHARE);
        c.retain(|l| l.2 >= floor);
        let mut chosen: Vec<&(bool, usize, f64, String)> = Vec::new();
        for l in c {
            if chosen.len() == max {
                break;
            }
            if !chosen.iter().any(|x| x.3 == l.3) {
                chosen.push(l);
            }
        }
        chosen.sort_by_key(|l| l.1);
        chosen.into_iter().map(|l| l.3.clone()).collect()
    };
    let asked = pick(true, MAX_USER_LINES);
    let answered = pick(false, MAX_ASSISTANT_LINES);

    let mut ranked: Vec<(&str, f64)> = weight.iter().map(|(t, w)| (*t, *w)).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    let keywords: Vec<&str> = ranked.into_iter().take(MAX_KEYWORDS).map(|(t, _)| t).collect();

    let mut out = format!("Session {}", span(first.at_ms, last.at_ms));
    if let Some(w) = workspace.map(str::trim).filter(|w| !w.is_empty()) {
        out.push_str(&format!(", in {w}"));
    }
    out.push_str(&format!(": {title}"));
    if !asked.is_empty() {
        out.push_str(&format!("\nAsked: {}", asked.join(" / ")));
    }
    if !answered.is_empty() {
        out.push_str(&format!("\nAnswered: {}", answered.join(" / ")));
    }
    if !keywords.is_empty() {
        out.push_str(&format!("\nKeywords: {}", keywords.join(", ")));
    }
    Some(out)
}

/// `on Tuesday 6 October 2026, 14:02–15:40`, or both ends in full when the session crossed
/// midnight. Local time.
pub(crate) fn span(from_ms: i64, to_ms: i64) -> String {
    let (Some(a), Some(b)) = (Local.timestamp_millis_opt(from_ms).single(), Local.timestamp_millis_opt(to_ms).single()) else {
        return String::new();
    };
    let day = "%A %-d %B %Y";
    if a.date_naive() == b.date_naive() {
        format!("on {}, {}–{}", a.format(day), a.format("%H:%M"), b.format("%H:%M"))
    } else {
        format!("from {} {} to {} {}", a.format(day), a.format("%H:%M"), b.format(day), b.format("%H:%M"))
    }
}

/// The first non-blank line's first sentence.
fn first_sentence(text: &str) -> String {
    sentences(text).into_iter().next().unwrap_or_default()
}

/// Sentences of prose: code fences skipped, list markers and headings stripped, split after
/// `.`, `?` or `!` followed by a space.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || l.is_empty() || l.starts_with('|') {
            continue;
        }
        let l = l.trim_start_matches(['-', '*', '#', '>', ' ']).trim();
        let mut cur = String::new();
        let mut chars = l.chars().peekable();
        while let Some(c) = chars.next() {
            cur.push(c);
            if matches!(c, '.' | '?' | '!') && chars.peek().is_some_and(|n| n.is_whitespace()) {
                push_sentence(&mut out, &cur);
                cur.clear();
            }
        }
        push_sentence(&mut out, &cur);
    }
    out
}

fn push_sentence(out: &mut Vec<String>, s: &str) {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if !s.is_empty() {
        out.push(s);
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Duration, Weekday};

    fn tuesday_at(h: u32, m: u32) -> i64 {
        let mut d = Local::now().date_naive();
        while d.weekday() != Weekday::Tue {
            d -= Duration::days(1);
        }
        Local.from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap()).single().unwrap().timestamp_millis()
    }

    /// Every term is rare except the ones a whole store would share.
    fn df(terms: &[&str]) -> Vec<usize> {
        terms.iter().map(|t| if ["think", "good", "looks"].contains(t) { 900 } else { 1 }).collect()
    }

    #[test]
    fn an_episode_is_the_opening_request_its_day_and_its_salient_lines() {
        let turns = [
            Turn { user: true, at_ms: tuesday_at(14, 2), text: "Plan the gate ablation for the reranker. I think it looks good." },
            Turn {
                user: false,
                at_ms: tuesday_at(14, 3),
                text: "Sure.\n```rust\nlet x = 1;\n```\nThe gate ablation drops one reranker feature per run and compares R@5.",
            },
            Turn { user: true, at_ms: tuesday_at(15, 40), text: "Drop the lexical feature first, then the dense cosine feature." },
        ];
        let text = compose(&turns, Some(r"C:\work\marlowe"), 1000, df).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("Session on Tuesday "), "{text}");
        assert!(first.contains(", 14:02–15:40, in C:\\work\\marlowe: Plan the gate ablation for the reranker."), "{text}");
        assert!(text.contains("\nAsked: Drop the lexical feature first, then the dense cosine feature."), "{text}");
        assert!(text.contains("\nAnswered: The gate ablation drops one reranker feature per run and compares R@5."), "{text}");
        assert!(!text.contains("let x"), "code is not prose: {text}");
        let keywords = text.lines().find_map(|l| l.strip_prefix("Keywords: ")).unwrap();
        assert!(keywords.contains("ablation") && keywords.contains("reranker"), "{keywords}");
        assert!(!text.contains("looks good"), "a line of common words is not salient: {text}");
        let keywords: Vec<&str> = keywords.split(", ").collect();
        assert!(!keywords.contains(&"think") && !keywords.contains(&"the"), "common words weigh nothing: {keywords:?}");
    }

    #[test]
    fn no_turns_no_episode_and_long_lines_are_clipped() {
        assert!(compose(&[], None, 10, df).is_none());
        let long = "word ".repeat(80);
        let turns = [Turn { user: true, at_ms: tuesday_at(9, 0), text: &long }];
        let text = compose(&turns, None, 10, df).unwrap();
        assert!(text.lines().next().unwrap().ends_with('…'), "{text}");
        assert!(!text.contains(", in "), "no workspace, no place: {text}");
    }

    #[test]
    fn a_session_over_midnight_names_both_days() {
        let s = span(tuesday_at(23, 30), tuesday_at(23, 30) + 3_600_000);
        assert!(s.starts_with("from Tuesday ") && s.contains(" to Wednesday "), "{s}");
    }
}
