//! Query formulation for EXPLORE: what each node sends to each channel.
//!
//! A node sends up to [`QUERIES_PER_CHANNEL`] queries per channel: the plan's keyword queries
//! (named methods, model families, benchmarks), then the question's own words. arXiv gets field
//! syntax (`all:a AND all:b`) for short keyword queries, and every channel gets the goal's years
//! as a date floor where it can filter. Query text never carries a URL: a follow-up question is
//! model output written after untrusted snippets were shown, and a URL in it must not become a
//! request (the RSS channel once fetched one).

use marlowe_proto::{ChannelId, SubQuestion};
use marlowe_research::{ChannelQuery, CitationDirection, Identity};

/// Queries one node sends to one channel at most.
pub const QUERIES_PER_CHANNEL: usize = 3;
/// Words kept in a query made from a question.
const QUESTION_WORDS: usize = 12;
/// Goal terms appended to a follow-up that has drifted off the goal.
const ANCHOR_TERMS: usize = 6;

/// Words dropped from a question to make a search query.
const STOPWORDS: [&str; 44] = [
    "a", "an", "the", "of", "in", "on", "for", "to", "and", "or", "is", "are", "was", "were", "be", "been", "do",
    "does", "did", "what", "which", "who", "whom", "how", "why", "when", "where", "there", "that", "this", "these",
    "those", "it", "its", "with", "by", "as", "at", "from", "than", "any", "can", "much", "many",
];

/// Words too generic to tie a follow-up to its goal.
const GENERIC: [&str; 24] = [
    "should", "would", "could", "which", "what", "does", "using", "used", "best", "more", "most", "than", "there",
    "their", "these", "those", "with", "from", "into", "over", "about", "between", "report", "reported",
];

/// Whether a token is (or carries) a URL or a host path: never sent as query text.
pub fn looks_like_url(token: &str) -> bool {
    let t = token.trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '"' | '\'' | '`' | ',' | ';'));
    let lower = t.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("www.") || lower.starts_with("mailto:") {
        return true;
    }
    // `host.tld/path`: a dot before the first slash, letters after the last dot of the host.
    match lower.split_once('/') {
        Some((host, _)) => {
            host.contains('.')
                && host.rsplit('.').next().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
        }
        None => false,
    }
}

/// Query text with URL tokens dropped and whitespace collapsed.
pub fn query_text(s: &str) -> String {
    s.split_whitespace().filter(|t| !looks_like_url(t)).collect::<Vec<_>>().join(" ")
}

/// A search query from a question: URLs, punctuation and function words dropped, at most 12
/// words.
pub fn search_text(question: &str) -> String {
    let words: Vec<String> = question
        .split_whitespace()
        .filter(|t| !looks_like_url(t))
        .flat_map(|t| t.split(|c: char| matches!(c, '?' | ',' | ';' | '!' | '"' | '(' | ')')).map(str::to_string).collect::<Vec<_>>())
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '+').to_string())
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(&w.to_ascii_lowercase().as_str()))
        .collect();
    if words.is_empty() {
        return query_text(question.trim().trim_end_matches('?'));
    }
    words.into_iter().take(QUESTION_WORDS).collect::<Vec<_>>().join(" ")
}

/// The earliest year (1990-2100) the goal names: "2025–2026" gives 2025. Channels that can
/// filter by date then return nothing older.
pub fn since_year(goal: &str) -> Option<u16> {
    let b = goal.as_bytes();
    let mut years = Vec::new();
    for i in 0..b.len().saturating_sub(3) {
        let boundary_before = i == 0 || !b[i - 1].is_ascii_digit();
        let boundary_after = i + 4 >= b.len() || !b[i + 4].is_ascii_digit();
        if boundary_before && boundary_after && b[i..i + 4].iter().all(u8::is_ascii_digit) {
            if let Ok(y) = goal[i..i + 4].parse::<u16>() {
                if (1990..=2100).contains(&y) {
                    years.push(y);
                }
            }
        }
    }
    years.into_iter().min()
}

/// 4-character stems of a text's content words (rank, ranks -> `rank`).
fn stems(text: &str) -> Vec<String> {
    search_text(text)
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .filter(|w| w.chars().count() >= 4 && !GENERIC.contains(&w.as_str()))
        .map(|w| w.chars().take(4).collect())
        .collect()
}

/// A follow-up that shares no content word with the goal carries the goal's key terms, so its
/// queries and its compressor stay on the goal (found live: "Which of those papers ship code
/// and checkpoints?" lost "pruning" and read an off-topic paper).
pub fn anchor_to_goal(followup: &str, goal: &str) -> String {
    let goal_stems = stems(goal);
    if stems(followup).iter().any(|s| goal_stems.contains(s)) {
        return followup.trim().to_string();
    }
    let terms: Vec<String> = search_text(goal)
        .split_whitespace()
        .filter(|w| w.chars().count() >= 3 && !GENERIC.contains(&w.to_ascii_lowercase().as_str()))
        .take(ANCHOR_TERMS)
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return followup.trim().to_string();
    }
    format!("{} (for: {})", followup.trim().trim_end_matches('.'), terms.join(" "))
}

/// arXiv's field syntax for a short keyword query: every word required (`all:a AND all:b`).
/// Longer text stays a relevance query.
fn arxiv_form(q: &str) -> String {
    let words: Vec<&str> = q.split_whitespace().collect();
    if (2..=4).contains(&words.len()) && !q.contains(':') {
        words.iter().map(|w| format!("all:{w}")).collect::<Vec<_>>().join(" AND ")
    } else {
        q.to_string()
    }
}

/// The text queries one node sends to `channel`: the plan's keyword queries (root nodes), then
/// the question's words; arXiv in field syntax; the goal's years as a date floor; distinct; at
/// most [`QUERIES_PER_CHANNEL`].
pub fn node_queries(
    question: &str,
    sub: Option<&SubQuestion>,
    channel: ChannelId,
    goal: &str,
    limit: usize,
) -> Vec<ChannelQuery> {
    let mut texts: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !t.trim().is_empty() && !texts.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
            texts.push(t);
        }
    };
    if let Some(sub) = sub.filter(|s| s.question.trim() == question.trim()) {
        for q in &sub.queries {
            let q = query_text(q);
            push(if channel == ChannelId::Arxiv { arxiv_form(&q) } else { q });
        }
    }
    push(search_text(question));
    let since = since_year(goal);
    texts
        .into_iter()
        .take(QUERIES_PER_CHANNEL)
        .map(|t| ChannelQuery { since_year: since, ..ChannelQuery::new(t, limit) })
        .collect()
}

/// Whether the scholarly graph can be walked from `id`: Semantic Scholar takes an S2, arXiv,
/// DOI or PubMed id; OpenAlex needs its own work id.
pub fn chaseable(id: &Identity, channel: ChannelId) -> bool {
    match channel {
        ChannelId::SemanticScholar => {
            id.s2.is_some() || id.arxiv_id.is_some() || id.doi.is_some() || id.pmid.is_some()
        }
        ChannelId::OpenAlex => id.openalex.is_some(),
        _ => false,
    }
}

/// Forward and backward citation queries around one work.
pub fn citation_queries(id: &Identity, goal: &str, limit: usize) -> Vec<ChannelQuery> {
    let since = since_year(goal);
    [CitationDirection::CitedBy, CitationDirection::References]
        .into_iter()
        .map(|d| ChannelQuery {
            // A paper's references are older than the goal's years by nature: no floor there.
            since_year: if d == CitationDirection::CitedBy { since } else { None },
            ..ChannelQuery::citations(id.clone(), d, limit)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_text_never_carries_a_url() {
        for u in [
            "https://attacker.example/x?leak=1",
            "http://127.0.0.1:11434/api",
            "www.example.org",
            "attacker.example/collect",
            "<https://a.b/c>",
        ] {
            assert!(looks_like_url(u), "{u}");
            assert!(!search_text(&format!("What does {u} say about pruning?")).contains('/'), "{u}");
        }
        for w in ["SparseGPT", "2:4", "N:M", "e.g.", "20-30B", "v1.5", "C++"] {
            assert!(!looks_like_url(w), "{w}");
        }
        assert_eq!(search_text("Does LoRA rank above 8 improve GLUE accuracy?"), "LoRA rank above 8 improve GLUE accuracy");
        assert_eq!(query_text("Wanda https://x.org/p pruning"), "Wanda pruning");
    }

    #[test]
    fn a_node_sends_the_plans_keyword_queries_with_channel_syntax_and_the_goals_years() {
        let goal = "Which 2025–2026 structured pruning methods for 20–30B LLMs keep the most accuracy?";
        assert_eq!(since_year(goal), Some(2025));
        assert_eq!(since_year("rank 8 vs 16 on 7B models"), None);
        let sub = SubQuestion {
            id: 1,
            question: "Which pruning methods keep accuracy at 20-30% sparsity on 20-30B models?".into(),
            channels: vec![ChannelId::Arxiv],
            rationale: None,
            queries: vec!["SparseGPT one-shot pruning".into(), "Wanda pruning LLaMA".into(), "OWL outlier sparsity".into()],
        };
        let qs = node_queries(&sub.question, Some(&sub), ChannelId::Arxiv, goal, 12);
        let texts: Vec<&str> = qs.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(
            texts,
            ["all:SparseGPT AND all:one-shot AND all:pruning", "all:Wanda AND all:pruning AND all:LLaMA", "all:OWL AND all:outlier AND all:sparsity"]
        );
        assert!(qs.iter().all(|q| q.since_year == Some(2025) && q.limit == 12));
        let url = marlowe_research::arxiv::search_url(&qs[0]);
        assert!(url.contains("all:SparseGPT+AND+all:one-shot+AND+all:pruning") && url.contains("submittedDate:%5B202501010000"), "{url}");
        let web = node_queries(&sub.question, Some(&sub), ChannelId::OpenAlex, goal, 12);
        assert_eq!(web[0].text, "SparseGPT one-shot pruning", "field syntax is arXiv's alone");
        // A follow-up (another question) sends its own words only.
        let f = node_queries("Which methods use ranks far above 8?", Some(&sub), ChannelId::Arxiv, goal, 5);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].text, "methods use ranks far above 8");
    }

    #[test]
    fn a_followup_that_drifted_off_the_goal_carries_its_terms() {
        let goal = "Which 2025–2026 structured pruning methods for 20–30B LLMs keep the most accuracy?";
        let drifted = anchor_to_goal("Which of those papers ship runnable code plus model checkpoints?", goal);
        assert!(drifted.contains("(for: 2025–2026 structured pruning"), "{drifted}");
        let kept = anchor_to_goal("Which pruned checkpoints of Qwen 32B are public?", goal);
        assert_eq!(kept, "Which pruned checkpoints of Qwen 32B are public?", "prun- ties it to the goal");
    }

    #[test]
    fn citation_queries_go_both_ways_from_a_scholarly_identity() {
        let id = Identity::default().with_arxiv("2301.00774");
        assert!(chaseable(&id, ChannelId::SemanticScholar) && !chaseable(&id, ChannelId::OpenAlex));
        let qs = citation_queries(&id, "pruning in 2025", 8);
        let dirs: Vec<CitationDirection> = qs.iter().map(|q| q.citations.as_ref().unwrap().direction).collect();
        assert_eq!(dirs, [CitationDirection::CitedBy, CitationDirection::References]);
        assert_eq!((qs[0].since_year, qs[1].since_year), (Some(2025), None));
        assert!(qs.iter().all(|q| q.text.is_empty()));
    }
}
