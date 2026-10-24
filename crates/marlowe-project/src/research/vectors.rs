//! AttackVectors: one employee per question writes where to look; code turns that into searches.
//!
//! The employee is told today's date and that its knowledge is old (the thing asked about may have
//! a release it has never heard of), and is asked for angles plus the names it suspects exist
//! ("qwen4?"). It writes plain search words. Code strips anything url-shaped, drops channels that
//! are not online, and adds what a model cannot be trusted to remember: the question's own words,
//! date-worded variants (the year, the month and year, "latest") and a year floor for channels
//! that can filter, because the web channel biases to recent results only within a year of now.

use std::sync::Arc;

use marlowe_model::SystemBlock;
use marlowe_research::{looks_like_url, query_text, search_text, since_year, ChannelId};
use serde_json::{json, Value};

use super::{object, plain, plain_text, question_of, ResearchDeps};
use crate::engine::fence;
use crate::engine::{Finished, Prompt, Toolbox};

/// The most search rows one question gets.
pub const MAX_QUERIES: usize = 36;

/// The channel groups a vector row may name, and the channels each stands for.
pub fn group_channels(group: &str, angle: &str) -> Vec<ChannelId> {
    let mut v = match group {
        "scholarly" => vec![ChannelId::OpenAlex, ChannelId::SemanticScholar],
        "arxiv" => vec![ChannelId::Arxiv],
        "community" => vec![ChannelId::HackerNews, ChannelId::StackExchange],
        "feeds" => vec![ChannelId::Rss],
        "wiki" => vec![ChannelId::Wikipedia],
        _ => vec![ChannelId::Web],
    };
    if angle == "code" {
        v.push(ChannelId::Github);
    }
    v
}

/// The group a channel belongs to (the other direction of [`group_channels`]).
pub fn channel_group(c: ChannelId) -> &'static str {
    match c {
        ChannelId::OpenAlex | ChannelId::SemanticScholar | ChannelId::Crossref | ChannelId::Pubmed => "scholarly",
        ChannelId::Arxiv => "arxiv",
        ChannelId::HackerNews | ChannelId::StackExchange | ChannelId::Github => "community",
        ChannelId::Rss => "feeds",
        ChannelId::Wikipedia | ChannelId::Wikidata => "wiki",
        _ => "web",
    }
}

/// One search to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRow {
    /// The channel.
    pub channel: ChannelId,
    /// The query text (never carries a url).
    pub text: String,
    /// `base`, `dated` or `unknown`.
    pub kind: &'static str,
    /// Only results on or after this year, where the channel can filter.
    pub since_year: Option<u16>,
}

/// One angle the employee wrote, cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Angle {
    /// `official`, `news`, `paper`, `code`, `discussion`, `benchmark`, `other`.
    pub angle: String,
    /// Plain search words.
    pub keywords: String,
    /// A channel group.
    pub group: String,
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// Year and month name of an ISO date (`2026-10-23`).
fn date_parts(today: &str) -> Option<(u16, &'static str)> {
    let mut it = today.split('-');
    let y: u16 = it.next()?.parse().ok()?;
    let m: usize = it.next()?.parse().ok()?;
    Some((y, MONTHS.get(m.checked_sub(1)?)?))
}

/// The searches for one question: the employee's angles, the question's own words (always), what
/// the employee suspects exists, then date-worded variants. Only `online` channels are used; an
/// angle for a channel that is not online goes to the web channel when that is.
pub fn expand(question: &str, angles: &[Angle], unknown: &[String], today: &str, online: &[ChannelId]) -> Vec<QueryRow> {
    let mut rows: Vec<QueryRow> = Vec::new();
    let floor = since_year(question);
    let date = date_parts(today);
    let recent = date.map(|(y, _)| y.saturating_sub(1));
    let push = |rows: &mut Vec<QueryRow>, channel: ChannelId, text: String, kind: &'static str, since: Option<u16>| {
        let text = text.trim().to_string();
        // A query needs words: found live, a name of "[1," went to arXiv as a search.
        let letters = text.chars().filter(|c| c.is_alphabetic()).count();
        if letters < 3 || rows.iter().any(|r| r.channel == channel && r.text.eq_ignore_ascii_case(&text)) {
            return;
        }
        rows.push(QueryRow { channel, text, kind, since_year: since });
    };
    let pick = |wanted: Vec<ChannelId>| -> Vec<ChannelId> {
        let got: Vec<ChannelId> = wanted.iter().copied().filter(|c| online.contains(c)).collect();
        if !got.is_empty() {
            got
        } else if online.contains(&ChannelId::Web) {
            vec![ChannelId::Web]
        } else {
            online.iter().copied().take(1).collect()
        }
    };
    let own = search_text(question);
    let mut bases: Vec<String> = Vec::new();
    for a in angles {
        for ch in pick(group_channels(&a.group, &a.angle)) {
            let text = if ch == ChannelId::Arxiv { arxiv_form(&a.keywords) } else { a.keywords.clone() };
            push(&mut rows, ch, text, "base", floor);
        }
        if !bases.iter().any(|b| b.eq_ignore_ascii_case(&a.keywords)) {
            bases.push(a.keywords.clone());
        }
    }
    for ch in pick(vec![ChannelId::Web]) {
        push(&mut rows, ch, own.clone(), "base", floor);
    }
    if !own.is_empty() && !bases.iter().any(|b| b.eq_ignore_ascii_case(&own)) {
        bases.push(own);
    }
    let web: Vec<ChannelId> = pick(vec![ChannelId::Web]);
    for n in unknown {
        let name = n.trim().trim_end_matches('?').trim().to_string();
        if name.is_empty() {
            continue;
        }
        for ch in &web {
            push(&mut rows, *ch, format!("{name} release"), "unknown", recent);
            if let Some((y, _)) = date {
                push(&mut rows, *ch, format!("{name} {y}"), "unknown", recent);
            }
        }
    }
    if let Some((y, month)) = date {
        for b in &bases {
            for ch in &web {
                push(&mut rows, *ch, format!("{b} {y}"), "dated", recent);
                push(&mut rows, *ch, format!("{b} {month} {y}"), "dated", recent);
                push(&mut rows, *ch, format!("{b} latest"), "dated", recent);
            }
        }
    }
    rows.truncate(MAX_QUERIES);
    rows
}

/// The names an objective is about: words with a capital letter after the first word, or with a
/// digit (Rust, TUI, GitHub, Qwen4, DeepSeek), at most four, in order.
fn subject_names(objective: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, w) in objective.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"')).filter(|w| !w.is_empty()).enumerate() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        let named = (i > 0 && w.chars().any(char::is_uppercase)) || (w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(char::is_alphabetic));
        if named && w.chars().count() >= 2 && !out.iter().any(|o| o.eq_ignore_ascii_case(w)) {
            out.push(w.to_string());
        }
        if out.len() == 4 {
            break;
        }
    }
    out
}

/// `question` with the objective's names in front when it names none of them.
fn with_subject(question: &str, objective: &str) -> String {
    let lower = question.to_lowercase();
    let names = subject_names(objective);
    if names.is_empty() || names.iter().any(|n| lower.contains(&n.to_lowercase())) {
        return question.to_string();
    }
    format!("{} {question}", names.join(" "))
}

fn arxiv_form(q: &str) -> String {
    let words: Vec<&str> = q.split_whitespace().collect();
    if (2..=4).contains(&words.len()) && !q.contains(':') {
        words.iter().map(|w| format!("all:{w}")).collect::<Vec<_>>().join(" AND ")
    } else {
        q.to_string()
    }
}

/// Registers the AttackVectors prompt and finish function.
pub fn register(t: &mut Toolbox, deps: &Arc<ResearchDeps>) {
    t.prompt("attack-vectors", |input| {
        let online_note = "Channels: web (general search), scholarly (papers, OpenAlex, Semantic Scholar), arxiv, community (Hacker News, Stack Exchange), wiki, feeds. An angle named `code` also searches GitHub.";
        let system = format!(
            "You are the search planner on a research project. Do exactly one task: say where to look for ONE question, and answer with the form only, a JSON object.\n\
             Objective: {}\n\
             Today is {}. Your own knowledge ends well before today and may be more than a year old. What is asked about may have newer versions, releases or results than you know of. \
             Make one angle aim at what you do not know, and list in `unknown_names` any names or versions you suspect exist but cannot confirm (a newer version, a successor), each with a question mark.\n\
             {online_note}\n\
             Every angle's keywords name the subject from the objective, even when the question only says \"each\" or \"they\". Keywords are plain search words, never web addresses. Write at most {} angles.",
            fence::clean(&input.plan.objective, 500, false),
            input.today,
            input.plan.limits.vectors_per_question
        );
        let q = input.item.as_ref().map(|i| super::s(&i.data, "text").to_string()).unwrap_or_default();
        Ok(Prompt::new(vec![SystemBlock { text: system, cache: true }], fence::fence("The question", &q, 400)))
    });

    let deps = deps.clone();
    t.finish("vectors_finish", move |input, map| {
        let Some(map) = map else { return Ok(Finished::default()) };
        let online = deps.web.channels();
        let q_item = input.item.as_ref();
        let question = q_item.map(|i| super::s(&i.data, "text").to_string()).unwrap_or_else(|| question_of(&input.plan, &input.sub));
        let mut rejected = super::Rejected::default();
        let mut angles: Vec<Angle> = Vec::new();
        for row in super::rows(&map, "vectors") {
            let keywords_raw = super::s(row, "keywords");
            if keywords_raw.split_whitespace().any(looks_like_url) {
                rejected.add("urls_in_keywords", 1);
                continue;
            }
            let keywords = query_text(&plain_text(keywords_raw, 80));
            if keywords.is_empty() {
                rejected.add("empty_keywords", 1);
                continue;
            }
            angles.push(Angle { angle: super::s(row, "angle").to_string(), keywords, group: super::s(row, "channel").to_string() });
        }
        angles.truncate(input.plan.limits.vectors_per_question as usize);
        let unknown: Vec<String> = map
            .get("unknown_names")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|n| {
                let (c, dropped) = plain(n, 40);
                if dropped {
                    rejected.add("urls_in_names", 1);
                }
                (!c.is_empty()).then_some(c)
            })
            .take(5)
            .collect();
        // A question that names none of the objective's subjects ("Under what licenses is each
        // published?") searches for nothing in particular: its own query carries them.
        let searched = with_subject(&question, &input.plan.objective);
        let queries = expand(&searched, &angles, &unknown, &input.today, &online);
        let vectors: Vec<Value> = angles
            .iter()
            .map(|a| json!({"angle": a.angle, "keywords": a.keywords, "channel": a.group, "why": ""}))
            .collect();
        // The employee's own `why` text, cleaned, goes back on the rows it wrote.
        let mut vectors = vectors;
        for (v, row) in vectors.iter_mut().zip(super::rows(&map, "vectors")) {
            v["why"] = Value::String(plain_text(super::s(row, "why"), 120));
        }
        let mut data = serde_json::Map::new();
        data.insert("question".into(), json!(question));
        data.insert("today".into(), json!(input.today));
        data.insert("vectors".into(), Value::Array(vectors));
        data.insert("unknown_names".into(), json!(unknown));
        data.insert(
            "queries".into(),
            Value::Array(
                queries
                    .iter()
                    .map(|r| json!({"channel": r.channel.as_str(), "query": r.text, "kind": r.kind, "since_year": r.since_year}))
                    .collect(),
            ),
        );
        Ok(Finished { data: object(Value::Object(data)), label: Some(format!("Angles: {}", fence::clean(&question, 80, false))), rejected: rejected.into_value(), ..Default::default() })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn angle(a: &str, k: &str, g: &str) -> Angle {
        Angle { angle: a.into(), keywords: k.into(), group: g.into() }
    }

    const ALL: [ChannelId; 6] = [ChannelId::Web, ChannelId::Arxiv, ChannelId::OpenAlex, ChannelId::SemanticScholar, ChannelId::Wikipedia, ChannelId::HackerNews];

    #[test]
    fn the_questions_own_words_are_always_searched_even_when_the_model_wrote_nothing_usable() {
        let rows = expand("Is Qwen4 released or announced, and what is known about it?", &[], &[], "2026-10-23", &ALL);
        assert!(rows.iter().any(|r| r.channel == ChannelId::Web && r.kind == "base" && r.text.to_lowercase().contains("qwen4")), "{rows:?}");
    }

    #[test]
    fn dated_variants_carry_the_year_the_month_and_latest_and_a_year_floor() {
        let rows = expand("Which Kimi models are newest?", &[angle("news", "kimi k3 release", "web")], &[], "2026-10-23", &ALL);
        let dated: Vec<&str> = rows.iter().filter(|r| r.kind == "dated").map(|r| r.text.as_str()).collect();
        assert!(dated.contains(&"kimi k3 release 2026"), "{dated:?}");
        assert!(dated.contains(&"kimi k3 release October 2026"));
        assert!(dated.contains(&"kimi k3 release latest"));
        assert!(rows.iter().filter(|r| r.kind == "dated").all(|r| r.since_year == Some(2025)), "a floor one year back");
        assert!(rows.iter().filter(|r| r.kind == "base").all(|r| r.since_year.is_none()), "base queries have no floor unless the question names a year");
    }

    #[test]
    fn a_question_that_names_no_subject_is_searched_with_the_objectives_names() {
        let objective = "Research and compare three most popular Rust TUI (Terminal User Interface) libraries by GitHub stars";
        assert_eq!(subject_names(objective), ["Rust", "TUI", "Terminal", "User"]);
        assert_eq!(with_subject("Under what open source licenses is each published?", objective), "Rust TUI Terminal User Under what open source licenses is each published?");
        assert_eq!(with_subject("How many GitHub stars do the top 3 Rust TUI libraries have?", objective), "How many GitHub stars do the top 3 Rust TUI libraries have?");
        assert_eq!(subject_names("Focus on new Kimi models, deepseek, and qwen4"), ["Kimi", "qwen4"]);
    }

    #[test]
    fn a_year_the_question_names_becomes_the_floor_of_the_base_queries() {
        let rows = expand("Which models were released in 2025 or 2026?", &[angle("news", "new models", "web")], &[], "2026-10-23", &ALL);
        assert!(rows.iter().filter(|r| r.kind == "base").all(|r| r.since_year == Some(2025)));
    }

    #[test]
    fn names_the_model_suspects_exist_become_queries() {
        let rows = expand("Is Qwen4 out?", &[], &["qwen4?".into(), "kimi k3".into()], "2026-10-23", &ALL);
        let unk: Vec<&str> = rows.iter().filter(|r| r.kind == "unknown").map(|r| r.text.as_str()).collect();
        assert_eq!(unk, ["qwen4 release", "qwen4 2026", "kimi k3 release", "kimi k3 2026"]);
    }

    #[test]
    fn a_channel_that_is_not_online_is_replaced_by_the_web_and_arxiv_gets_field_syntax() {
        let online = [ChannelId::Web, ChannelId::Arxiv];
        let rows = expand("q", &[angle("paper", "sparse attention", "scholarly"), angle("paper", "sparse attention", "arxiv")], &[], "2026-10-23", &online);
        assert!(rows.iter().any(|r| r.channel == ChannelId::Web && r.text == "sparse attention" && r.kind == "base"));
        assert!(rows.iter().any(|r| r.channel == ChannelId::Arxiv && r.text == "all:sparse AND all:attention"));
        assert!(rows.iter().all(|r| online.contains(&r.channel)));
        assert!(expand("q", &[angle("paper", "x y", "wiki")], &[], "2026-10-23", &[]).is_empty(), "nothing online, nothing to search");
    }

    #[test]
    fn rows_are_distinct_and_capped() {
        let angles: Vec<Angle> = (0..6).map(|i| angle("news", &format!("topic number {i}"), "web")).collect();
        let unknown: Vec<String> = (0..5).map(|i| format!("name{i}?")).collect();
        let rows = expand("a long question about several different topics", &angles, &unknown, "2026-10-23", &ALL);
        assert!(rows.len() <= MAX_QUERIES);
        let mut seen = std::collections::HashSet::new();
        assert!(rows.iter().all(|r| seen.insert((r.channel, r.text.to_lowercase()))));
    }

    #[test]
    fn a_code_angle_also_searches_github_when_it_is_online() {
        let online = [ChannelId::Web, ChannelId::Github];
        let rows = expand("q", &[angle("code", "kimi inference engine", "web")], &[], "2026-10-23", &online);
        assert!(rows.iter().any(|r| r.channel == ChannelId::Github));
    }

    #[test]
    fn date_parts_read_an_iso_date() {
        assert_eq!(date_parts("2026-10-23"), Some((2026, "October")));
        assert_eq!(date_parts("nonsense"), None);
        assert_eq!(date_parts("2026-13-01"), None);
    }
}
