//! Dedup measured on real channel output (ADR-036 §6): routes recorded live on 2026-10-09 for
//! three works through arXiv, OpenAlex, Crossref, PubMed, Wikipedia and Hacker News, hand-labelled.
//! Merge precision and recall are reported separately, against a negative control (look-alike
//! titles that must not merge) and an independence control (a Wikipedia article and an HN thread
//! about a paper are one root with it, not three).
//!
//!   cargo test -p marlowe-research --test dedup_eval -- --nocapture
//!
//! What the corpus holds that a synthetic one would not: Crossref has five `10.65215/...`
//! posted-content DOIs that repost "Attention Is All You Need" (same authors and abstract, dated
//! 2025), and OpenAlex's canonical record for the paper (W2626778328) carries one of those DOIs
//! and a 2025 date while its arXiv location says 1706.03762.

use std::collections::{BTreeMap, BTreeSet};

use marlowe_research::channels::{arxiv, community, pubmed, scholarly, wiki};
use marlowe_research::{ChannelId, Deduper, DerivationGraph, Resolution, SourceId, SourceRef};
use serde_json::Value;

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn json(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> Vec<&'a Value> {
    let mut cur = v;
    for k in path {
        cur = &cur[*k];
    }
    cur.as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

/// Every route in the corpus, in the order a run would see them: indexes first, authorities
/// later (the harder order: merges must happen on the authority's arrival).
fn corpus() -> Vec<SourceRef> {
    let mut out = Vec::new();
    for f in [
        "corpus/openalex_attention.json",
        "corpus/openalex_lora.json",
        "openalex_search.json",
        "corpus/openalex_deeplearning.json",
    ] {
        out.extend(arr(&json(f), &["results"]).into_iter().filter_map(scholarly::openalex_work));
    }
    out.extend(arr(&json("corpus/hn_attention.json"), &["hits"]).into_iter().filter_map(community::hn_hit));
    out.extend(arr(&json("hn_search.json"), &["hits"]).into_iter().filter_map(community::hn_hit));
    for f in ["corpus/wikipedia_attention.json", "wikipedia_search.json"] {
        out.extend(wiki::wikipedia_pages(&json(f)));
    }
    let ids = |f: &str| -> Vec<String> {
        arr(&json(f), &["esearchresult", "idlist"]).iter().filter_map(|v| v.as_str().map(String::from)).collect()
    };
    out.extend(
        pubmed::parse_efetch(
            &fixture("corpus/pubmed_deeplearning_efetch.xml"),
            &ids("corpus/pubmed_deeplearning_esearch.json"),
        )
        .unwrap(),
    );
    out.extend(pubmed::parse_efetch(&fixture("pubmed_efetch.xml"), &ids("pubmed_esearch.json")).unwrap());
    out.extend(
        arr(&json("corpus/crossref_attention.json"), &["message", "items"])
            .into_iter()
            .filter_map(scholarly::crossref_item),
    );
    out.extend(
        arr(&json("crossref_search.json"), &["message", "items"]).into_iter().filter_map(scholarly::crossref_item),
    );
    out.extend(scholarly::crossref_item(&json("crossref_lookup.json")["message"]));
    for f in ["corpus/arxiv_attention.xml", "arxiv_lookup.xml", "arxiv_search.xml"] {
        out.extend(arxiv::parse_atom(&fixture(f)).entries.iter().map(arxiv::to_source));
    }
    out
}

/// Hand-labelled works with more than one route; every other route is its own work.
fn gold() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "Transformer (Vaswani et al.) and its 10.65215 reposts",
            vec![
                "arxiv|https://arxiv.org/abs/1706.03762",
                "open_alex|https://doi.org/10.65215/2q58a426",
                "crossref|https://doi.org/10.65215/r5bs2d54",
                "crossref|https://doi.org/10.65215/ysbyhc05",
                "crossref|https://doi.org/10.65215/pc26a033",
                "crossref|https://doi.org/10.65215/mdcm8z23",
                "crossref|https://doi.org/10.65215/nxvz2v36",
            ],
        ),
        (
            "Deep learning (Nature 2015)",
            vec!["crossref|https://doi.org/10.1038/nature14539", "pubmed|https://pubmed.ncbi.nlm.nih.gov/26017442/"],
        ),
        ("QA-LoRA (two OpenAlex answers)", vec!["open_alex|http://arxiv.org/abs/2309.14717"]),
        ("MiLO-LoRA (two OpenAlex answers)", vec!["open_alex|https://doi.org/10.5281/zenodo.20209068"]),
        ("Bayesian-LoRA (two OpenAlex answers)", vec!["open_alex|https://arxiv.org/abs/2601.21003"]),
    ]
}

/// Look-alike titles that are different works: none may merge with anything.
const NEGATIVE_CONTROL: [&str; 6] = [
    "open_alex|https://doi.org/10.5281/zenodo.18289747",
    "open_alex|https://doi.org/10.1109/icassp39728.2021.9413901",
    "open_alex|https://doi.org/10.1609/aaai.v34i07.6693",
    "open_alex|https://doi.org/10.1093/bib/bbad467",
    "arxiv|https://arxiv.org/abs/2104.04692",
    "wikipedia|https://en.wikipedia.org/wiki/Attention_Is_All_You_Need",
];

fn key(r: &SourceRef) -> String {
    format!("{}|{}", r.channel.as_str(), r.url)
}

struct Score {
    tp: usize,
    predicted: usize,
    actual: usize,
}

impl Score {
    fn precision(&self) -> f64 {
        if self.predicted == 0 {
            1.0
        } else {
            self.tp as f64 / self.predicted as f64
        }
    }
    fn recall(&self) -> f64 {
        if self.actual == 0 {
            1.0
        } else {
            self.tp as f64 / self.actual as f64
        }
    }
}

/// Pairwise merge scores: `cluster[i]` is route i's predicted cluster.
fn score(keys: &[String], cluster: &[usize], gold_of: &dyn Fn(&str) -> String) -> Score {
    let mut s = Score { tp: 0, predicted: 0, actual: 0 };
    for i in 0..keys.len() {
        for j in i + 1..keys.len() {
            let same_gold = gold_of(&keys[i]) == gold_of(&keys[j]);
            let same_pred = cluster[i] == cluster[j];
            s.actual += usize::from(same_gold);
            s.predicted += usize::from(same_pred);
            s.tp += usize::from(same_gold && same_pred);
        }
    }
    s
}

#[test]
fn dedup_on_recorded_channel_output() {
    let routes = corpus();
    let gold = gold();
    let mut label: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, members) in &gold {
        for m in members {
            label.insert(m, name);
        }
    }
    let gold_of = |k: &str| label.get(k).map_or(k.to_string(), |n| n.to_string());
    for (_, members) in &gold {
        for m in members {
            assert!(routes.iter().any(|r| key(r) == *m), "gold route {m} is in the corpus");
        }
    }

    let mut d = Deduper::default();
    let placements: Vec<_> = routes.iter().cloned().map(|r| d.add(r)).collect();
    let keys: Vec<String> = routes.iter().map(key).collect();
    let merged: Vec<usize> = placements.iter().map(|p| d.resolve(p.source).0 as usize).collect();
    // Counting view: unresolved pairs collapse (ADR-036 §4.3).
    let mut graph = DerivationGraph::new();
    for &(a, b) in d.unresolved() {
        graph.unresolved(a, b);
    }
    let counted: Vec<usize> =
        merged.iter().map(|&s| graph.roots_of(SourceId(s as u32)).into_iter().next().unwrap().0 as usize).collect();

    // Strawman: exact normalised-title equality, what a naive deduper does.
    let titles: Vec<String> = routes.iter().map(|r| marlowe_research::normalize_title(&r.title)).collect();
    let naive: Vec<usize> = titles.iter().map(|t| titles.iter().position(|x| x == t).unwrap()).collect();

    let m = score(&keys, &merged, &gold_of);
    let c = score(&keys, &counted, &gold_of);
    let n = score(&keys, &naive, &gold_of);
    let unresolved = placements.iter().filter(|p| p.resolution == Resolution::Unresolved).count();
    println!("dedup eval: {} routes -> {} sources ({} unresolved placements)", routes.len(), d.len(), unresolved);
    println!(
        "  merged (display):     precision {:.3}  recall {:.3}  ({} / {} / {})",
        m.precision(),
        m.recall(),
        m.tp,
        m.predicted,
        m.actual
    );
    println!(
        "  counted (corrob.):    precision {:.3}  recall {:.3}  ({} / {} / {})",
        c.precision(),
        c.recall(),
        c.tp,
        c.predicted,
        c.actual
    );
    println!(
        "  naive title equality: precision {:.3}  recall {:.3}  ({} / {} / {})",
        n.precision(),
        n.recall(),
        n.tp,
        n.predicted,
        n.actual
    );
    for p in d.unresolved() {
        println!(
            "  unresolved: {:?} [{}] ~ {:?} [{}]",
            p.0,
            d.get(p.0).unwrap().title(),
            p.1,
            d.get(p.1).unwrap().title()
        );
    }

    assert_eq!(m.precision(), 1.0, "no false merges on display");
    assert_eq!(m.recall(), 1.0, "every labelled duplicate merges");
    assert!(n.precision() < 1.0, "the control can fail: naive title matching merges look-alikes");

    // Negative control: none of these shares a source (or a counted root) with any other route.
    for k in NEGATIVE_CONTROL {
        let i = keys.iter().position(|x| x == k).unwrap_or_else(|| panic!("{k} in corpus"));
        let alone = (0..keys.len()).filter(|&j| j != i).all(|j| merged[j] != merged[i] && counted[j] != counted[i]);
        assert!(alone, "{k} merged with something");
    }

    // Independence control: the paper, its Wikipedia article and its HN thread are one root.
    let at = |k: &str| SourceId(merged[keys.iter().position(|x| x == k).unwrap()] as u32);
    let paper = at("arxiv|https://arxiv.org/abs/1706.03762");
    let article = at("wikipedia|https://en.wikipedia.org/wiki/Attention_Is_All_You_Need");
    let thread = at("hacker_news|https://news.ycombinator.com/item?id=15938082");
    graph.derives_from(article, paper);
    graph.derives_from(thread, paper);
    assert_eq!(graph.independent_roots(&[paper, article, thread]), 1);
    let copies: BTreeSet<SourceId> = gold[0].1.iter().map(|k| at(k)).collect();
    assert_eq!(copies.len(), 1, "the reposts are routes of one source, not five witnesses");
    let s = d.get(paper).unwrap();
    assert!(s.channels().contains(&ChannelId::Crossref) && s.channels().contains(&ChannelId::OpenAlex));
    assert!(!s.conflicts.is_empty(), "the 2017 preprint and the 2025 reposts disagree on the year, and that is kept");
}
