//! Credibility and relevance: numbers code owns.
//!
//! The model's only input is `source_kind` (what kind of source it judges the page to be). Code
//! caps it by where the page came from, so a page that claims to be official lifts its score to
//! its channel's cap at most; a retracted work scores 0. The table has a version, and its tests
//! pin every row.

use serde_json::Value;

/// Bump when a row of the table changes.
pub const TABLE_VERSION: u32 = 1;

/// The weight of a relevancy label: `Critical` 1.0, `High` 0.7, `Medium` 0.4, `Low` 0.15, `Skip` 0.
/// The same numbers are in the `analyze` form; a test keeps the two equal.
pub fn relevancy_weight(label: &str) -> f64 {
    match label {
        "Critical" => 1.0,
        "High" => 0.7,
        "Medium" => 0.4,
        "Low" => 0.15,
        _ => 0.0,
    }
}

/// The label one step lower (`Skip` stays).
pub fn lower(label: &str) -> &'static str {
    match label {
        "Critical" => "High",
        "High" => "Medium",
        "Medium" => "Low",
        _ => "Skip",
    }
}

/// What the page's origin says about it, all of it known to code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Origin {
    /// The channel it came from: `web`, `arxiv`, `open_alex`, `crossref`, `semantic_scholar`,
    /// `pubmed`, `wikipedia`, `wikidata`, `github`, `hacker_news`, `stack_exchange`, `rss`.
    pub channel: String,
    /// A channel's own record confirmed its DOI, PMID or arXiv id (not merely page text saying so).
    pub confirmed: bool,
    /// GitHub stars (from the channel's structured fields).
    pub stars: Option<u64>,
    /// The channel reports the work retracted.
    pub retracted: bool,
    /// The page had little text.
    pub thin: bool,
}

/// The ceiling a channel puts on a source of this kind.
pub fn channel_cap(o: &Origin, kind: &str) -> f64 {
    match o.channel.as_str() {
        "open_alex" | "crossref" | "semantic_scholar" | "pubmed" => {
            if o.confirmed {
                0.95
            } else {
                0.7
            }
        }
        "arxiv" => 0.8,
        "github" => {
            let s = o.stars.unwrap_or(0) as f64;
            0.6 + 0.2 * ((s + 1.0).log10() / 4.0).clamp(0.0, 1.0)
        }
        "wikipedia" | "wikidata" => 0.6,
        "hacker_news" | "stack_exchange" | "rss" => 0.5,
        _ => {
            if kind == "official" {
                0.7
            } else {
                0.5
            }
        }
    }
}

/// How much trust a source type earns before any cap.
pub fn kind_base(kind: &str) -> f64 {
    match kind {
        "official" | "paper" => 0.9,
        "repo" => 0.75,
        "news" => 0.6,
        "blog" => 0.45,
        "forum" => 0.35,
        "aggregator" => 0.3,
        _ => 0.3,
    }
}

/// The multiplier for what is doubtful about the page itself.
pub fn modifier(o: &Origin) -> f64 {
    if o.retracted {
        return 0.0;
    }
    let mut m = 1.0;
    // An identity the page text asserted but no channel record confirmed.
    if matches!(o.channel.as_str(), "open_alex" | "crossref" | "semantic_scholar" | "pubmed") && !o.confirmed {
        m *= 0.85;
    }
    if o.thin {
        m *= 0.8;
    }
    m
}

/// The best this page can score, before the model says what kind it is (shown on the page item).
pub fn ceiling(o: &Origin) -> f64 {
    round2(channel_cap(o, "official") * modifier(o))
}

/// The credibility of an analysed source: the model's `source_kind` can lower it below the
/// channel's cap and never raise it above.
pub fn credibility(o: &Origin, kind: &str) -> f64 {
    round2(channel_cap(o, kind).min(kind_base(kind)) * modifier(o))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The origin a stored page item records (`channel`, `identity_confirmed`, `stars`, `retracted`
/// and `status` in its data).
pub fn origin_of(page: &serde_json::Map<String, Value>) -> Origin {
    Origin {
        channel: page.get("channel").and_then(Value::as_str).unwrap_or("web").to_string(),
        confirmed: page.get("identity_confirmed").and_then(Value::as_bool).unwrap_or(false),
        stars: page.get("stars").and_then(Value::as_u64),
        retracted: page.get("retracted").and_then(Value::as_bool).unwrap_or(false),
        thin: page.get("status").and_then(Value::as_str) == Some("thin"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(channel: &str) -> Origin {
        Origin { channel: channel.into(), ..Default::default() }
    }

    #[test]
    fn the_table_is_pinned_row_by_row() {
        assert_eq!(TABLE_VERSION, 1);
        let confirmed = Origin { channel: "open_alex".into(), confirmed: true, ..Default::default() };
        assert_eq!(credibility(&confirmed, "paper"), 0.9, "min(0.95, 0.9)");
        assert_eq!(credibility(&Origin { channel: "arxiv".into(), ..Default::default() }, "paper"), 0.8);
        assert_eq!(credibility(&o("web"), "official"), 0.7);
        assert_eq!(credibility(&o("web"), "news"), 0.5, "a plain web page is capped at 0.5 unless official");
        assert_eq!(credibility(&o("web"), "blog"), 0.45);
        assert_eq!(credibility(&o("web"), "forum"), 0.35);
        assert_eq!(credibility(&o("web"), "aggregator"), 0.3);
        assert_eq!(credibility(&o("hacker_news"), "official"), 0.5, "the channel's cap wins over the model's claim");
        assert_eq!(credibility(&o("wikipedia"), "news"), 0.6);
    }

    #[test]
    fn the_model_can_lower_a_source_below_its_cap_and_never_raise_it_above() {
        for ch in ["web", "arxiv", "hacker_news", "wikipedia", "github", "open_alex"] {
            let origin = Origin { channel: ch.into(), ..Default::default() };
            for kind in ["official", "paper", "repo", "news", "blog", "forum", "aggregator", "other", "something-invented"] {
                assert!(credibility(&origin, kind) <= channel_cap(&origin, kind) + 1e-9, "{ch}/{kind}");
                assert!(credibility(&origin, kind) <= kind_base(kind) + 1e-9, "{ch}/{kind}");
            }
        }
        // `paper` on the plain web is bounded by the web cap of a source that is not official.
        assert_eq!(credibility(&o("web"), "paper"), 0.5);
    }

    #[test]
    fn retracted_scores_zero_and_doubts_multiply() {
        let retracted = Origin { channel: "open_alex".into(), confirmed: true, retracted: true, ..Default::default() };
        assert_eq!(credibility(&retracted, "paper"), 0.0);
        let unconfirmed = Origin { channel: "crossref".into(), ..Default::default() };
        assert_eq!(credibility(&unconfirmed, "paper"), 0.6, "0.7 x 0.85, rounded");
        let thin = Origin { channel: "web".into(), thin: true, ..Default::default() };
        assert_eq!(credibility(&thin, "official"), 0.56);
    }

    #[test]
    fn github_stars_move_the_cap_between_06_and_08() {
        let g = |s| Origin { channel: "github".into(), stars: Some(s), ..Default::default() };
        assert_eq!(channel_cap(&g(0), "repo"), 0.6);
        assert!(channel_cap(&g(100), "repo") > 0.69 && channel_cap(&g(100), "repo") < 0.71);
        assert_eq!(channel_cap(&g(1_000_000), "repo"), 0.8);
    }

    #[test]
    fn relevancy_weights_match_the_form_and_lowering_steps_down() {
        let reg = crate::registry::Registry::builtin();
        let form = reg.form("analyze@1").expect("the analyze form");
        let rel = form.fields.iter().find(|f| f.key == "relevancy").unwrap();
        for v in &rel.values {
            assert_eq!(v.weight.unwrap_or(0.0), relevancy_weight(&v.v), "{}", v.v);
        }
        assert_eq!([lower("Critical"), lower("High"), lower("Medium"), lower("Low"), lower("Skip")], ["High", "Medium", "Low", "Skip", "Skip"]);
    }
}
