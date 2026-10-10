use super::*;
use crate::Link;

const ARTICLE: &str = include_str!("../../tests/fixtures/article.html");
const DOCS: &str = include_str!("../../tests/fixtures/docs.html");
const FORUM: &str = include_str!("../../tests/fixtures/forum.html");
const SPA: &str = include_str!("../../tests/fixtures/spa_shell.html");

fn ex(c: Candidate, chars: usize, conf: Option<f32>) -> Extracted {
    Extracted {
        candidate: c,
        text: "x".repeat(chars),
        title: None,
        page_type: PageType::Article,
        confidence: conf,
        elapsed: Duration::ZERO,
    }
}

/// No time budget: debug builds under a parallel test run are far slower than the budget assumes.
fn read(html: &str, url: &str) -> MainText {
    Selector::with_policy(SelectionPolicy { budget: None, ..SelectionPolicy::default() })
        .read(html.as_bytes(), Some("text/html; charset=utf-8"), Some(url))
        .unwrap()
}

#[test]
fn a_confident_candidate_beats_the_baseline_and_an_unsure_one_does_not() {
    let p = SelectionPolicy::default();
    let c = choose(ex(Candidate::Baseline, 900, None), vec![Ok(ex(Candidate::RsTrafilatura, 800, Some(0.9)))], &p);
    assert_eq!((c.chosen.candidate, c.reason), (Candidate::RsTrafilatura, ChoiceReason::Preferred));
    let c = choose(ex(Candidate::Baseline, 900, None), vec![Ok(ex(Candidate::RsTrafilatura, 800, Some(0.2)))], &p);
    assert_eq!((c.chosen.candidate, c.reason), (Candidate::Baseline, ChoiceReason::Fallback));
}

#[test]
fn thin_failed_or_truncated_candidates_fall_back_and_all_thin_picks_the_longest() {
    let p = SelectionPolicy::default();
    let c = choose(
        ex(Candidate::Baseline, 600, None),
        vec![
            Ok(ex(Candidate::RsTrafilatura, 40, Some(0.99))),
            Err(MainContentError::Unavailable(Candidate::RustTrafilatura)),
        ],
        &p,
    );
    assert_eq!(c.chosen.candidate, Candidate::Baseline);
    assert_eq!(c.failures, vec![MainContentError::Unavailable(Candidate::RustTrafilatura)]);
    let c = choose(ex(Candidate::Baseline, 30, None), vec![Ok(ex(Candidate::RustTrafilatura, 90, None))], &p);
    assert_eq!((c.chosen.candidate, c.reason), (Candidate::RustTrafilatura, ChoiceReason::LongestOfThin));
    // 300 of a 40,000-char page is a truncation, not a reading.
    let c = choose(ex(Candidate::Baseline, 40_000, None), vec![Ok(ex(Candidate::RustTrafilatura, 300, None))], &p);
    assert_eq!(c.chosen.candidate, Candidate::Baseline);
}

#[test]
fn a_page_type_override_reorders_candidates() {
    let mut p = SelectionPolicy::default();
    p.by_type
        .push((PageType::Article, vec![Candidate::RustTrafilatura, Candidate::RsTrafilatura, Candidate::Baseline]));
    let c = choose(
        ex(Candidate::Baseline, 900, None),
        vec![Ok(ex(Candidate::RsTrafilatura, 800, Some(0.9))), Ok(ex(Candidate::RustTrafilatura, 700, None))],
        &p,
    );
    assert_eq!(c.chosen.candidate, Candidate::RustTrafilatura);
    assert_eq!(c.page_type, PageType::Article);
}

/// A candidate that counts its calls, to prove the selector stops early.
struct Counting(Candidate, usize, std::sync::atomic::AtomicUsize);
impl MainContent for Counting {
    fn candidate(&self) -> Candidate {
        self.0
    }
    fn extract(&self, _: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ex(self.0, self.1, Some(0.9)))
    }
}

#[test]
fn the_selector_stops_at_the_first_acceptable_candidate_and_survives_a_panic() {
    struct Panics;
    impl MainContent for Panics {
        fn candidate(&self) -> Candidate {
            Candidate::RsTrafilatura
        }
        fn extract(&self, _: &PageInput<'_>) -> Result<Extracted, MainContentError> {
            panic!("boom")
        }
    }
    let rust = std::sync::Arc::new(Counting(Candidate::RustTrafilatura, 900, Default::default()));
    let page = PageInput { html: "<p>short</p>", url: None };
    let base = ex(Candidate::Baseline, 1_000, None);
    let s = Selector::new(
        vec![Box::new(Panics), Box::new(Counting(Candidate::RustTrafilatura, 900, Default::default()))],
        SelectionPolicy::default(),
    );
    let c = s.choose_with(base.clone(), &page);
    assert_eq!(c.chosen.candidate, Candidate::RustTrafilatura);
    assert_eq!(c.failures, vec![MainContentError::Panicked(Candidate::RsTrafilatura)]);
    // rs acceptable first: rust never runs.
    let s = Selector::new(
        vec![Box::new(Counting(Candidate::RsTrafilatura, 900, Default::default())), Box::new(ArcCand(rust.clone()))],
        SelectionPolicy::default(),
    );
    assert_eq!(s.choose_with(base, &page).chosen.candidate, Candidate::RsTrafilatura);
    assert_eq!(rust.2.load(std::sync::atomic::Ordering::SeqCst), 0);
}

struct ArcCand(std::sync::Arc<Counting>);
impl MainContent for ArcCand {
    fn candidate(&self) -> Candidate {
        self.0.candidate()
    }
    fn extract(&self, p: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        self.0.extract(p)
    }
}

#[test]
fn the_baseline_only_selector_is_html_rs() {
    let m =
        Selector::baseline_only().read(ARTICLE.as_bytes(), Some("text/html"), Some("https://ex.org/p/lora")).unwrap();
    assert_eq!(m.choice.chosen.candidate, Candidate::Baseline);
    assert!(m.choice.failures.is_empty());
    let plain = crate::html::extract(ARTICLE.as_bytes(), Some("text/html"), Some("https://ex.org/p/lora")).unwrap();
    assert_eq!(m.document.text, plain.text, "baseline text is unchanged by the structured pass");
}

#[test]
fn chosen_text_maps_back_onto_blocks_and_renders_markdown() {
    let b = |kind, text: &str, links: Vec<usize>| BlockSpan {
        kind,
        text: text.into(),
        links,
        boilerplate: false,
        content: true,
    };
    let blocks = vec![
        b(BlockKind::Heading(2), "Results", vec![]),
        b(BlockKind::Paragraph, "We compare LoRA at rank 8 against rank 64 on three tasks.", vec![0]),
        b(BlockKind::ListItem, "Subscribe now for weekly posts", vec![]),
        b(BlockKind::Code, "r = 8\nalpha = 16", vec![]),
        b(BlockKind::TableCell { row: 1, header: true }, "Rank", vec![]),
        b(BlockKind::TableCell { row: 1, header: true }, "F1", vec![]),
        b(BlockKind::TableCell { row: 2, header: false }, "8", vec![]),
        b(BlockKind::TableCell { row: 2, header: false }, "0.91", vec![]),
    ];
    let chosen =
        "Results\nWe compare LoRA at rank 8 against rank 64 on three tasks.\nr = 8 alpha = 16\nRank F1\n8 0.91";
    let keep = align(chosen, &blocks);
    assert_eq!(keep, vec![0, 1, 3, 4, 5, 6, 7]);
    assert!(coverage(chosen, &blocks, &keep) > 0.95);
    let links = vec![Link { url: "https://arxiv.org/abs/2106.09685".into(), text: "LoRA".into() }];
    let md = to_markdown(&blocks, &keep, &links);
    assert!(md.starts_with("## Results"), "{md}");
    assert!(md.contains("We compare [LoRA](https://arxiv.org/abs/2106.09685) at rank 8"), "{md}");
    assert!(md.contains("```\nr = 8\nalpha = 16\n```"), "{md}");
    assert!(md.contains("| Rank | F1 |\n| --- | --- |\n| 8 | 0.91 |"), "{md}");
    assert!(!md.contains("Subscribe"));

    // Found by the eval's URL-level link recall: headings carry their links, and an anchor whose
    // whitespace differs from the collapsed block text still matches.
    let blocks = vec![
        b(BlockKind::Heading(3), "QLoRA: Efficient Finetuning", vec![0]),
        b(BlockKind::Paragraph, "See the low rank paper for details.", vec![1]),
    ];
    let links = vec![
        Link { url: "https://arxiv.org/abs/2305.14314".into(), text: "QLoRA".into() },
        Link { url: "https://arxiv.org/abs/2106.09685".into(), text: "low\n   rank paper".into() },
    ];
    let md = to_markdown(&blocks, &[0, 1], &links);
    assert!(md.starts_with("### [QLoRA](https://arxiv.org/abs/2305.14314): Efficient Finetuning"), "{md}");
    assert!(md.contains("the [low rank paper](https://arxiv.org/abs/2106.09685) for"), "{md}");
}

#[test]
fn article_fixture_keeps_structure_and_drops_chrome() {
    let m = read(ARTICLE, "https://notebook.example/p/lora");
    let md = &m.markdown;
    for want in [
        "LoRA](https://arxiv.org/abs/2106.09685)",
        "## Experimental setup",
        "| Rank | Trainable params | MMLU | MT-Bench |",
        "| 8 | 4.2M | 46.8 | 6.02 |",
        "    lora_alpha=16,",
        "> The intrinsic rank of the adaptation is low",
        "- Continued pretraining on a new domain",
        "[PEFT library](https://github.com/huggingface/peft)",
    ] {
        assert!(md.contains(want), "missing {want:?} ({:?}) in\n{md}", m.choice.chosen.candidate);
    }
    for chrome in ["Accept all", "All rights reserved", "Get new posts by email", "QLoRA on a single GPU"] {
        assert!(!md.contains(chrome), "chrome {chrome:?} survived ({:?}):\n{md}", m.choice.chosen.candidate);
    }
    assert!(m.structured && m.coverage >= MIN_COVERAGE, "coverage {}", m.coverage);
    assert!(!m.client_rendered);
}

#[test]
fn docs_fixture_keeps_both_code_examples_and_drops_the_sidebar() {
    let m = read(DOCS, "https://docs.rs/crossbeam-channel/latest/crossbeam_channel/macro.select.html");
    let md = &m.markdown;
    assert!(md.contains("Selects from a set of channel operations."), "{md}");
    assert!(md.contains("recv(r1) -> msg => assert_eq!(msg, Ok(10)),"), "{md}");
    assert!(md.contains("default(Duration::from_millis(100))"), "{md}");
    assert!(md.contains("Passing the same channel twice is allowed."), "{md}");
    assert!(!md.contains("tick"), "sidebar survived ({:?}):\n{md}", m.choice.chosen.candidate);
}

#[test]
fn forum_fixture_keeps_the_question_and_every_answer() {
    let m = read(FORUM, "https://users.rust-lang.org/t/thread-pool-deadlock/104233");
    let t = &m.document.text;
    for want in [
        "When one job panics, join never returns.",
        "Decrement in a drop guard so it runs during unwinding",
        "fixed the deadlock",
    ] {
        assert!(t.contains(want), "lost {want:?} ({:?} as {:?}):\n{t}", m.choice.chosen.candidate, m.choice.page_type);
    }
    assert!(!t.contains("Is crossbeam still needed"), "suggested topics survived:\n{t}");
}

#[test]
fn an_spa_shell_is_flagged_as_client_rendered() {
    let m = read(SPA, "https://lumen.example/docs/scheduler");
    assert!(m.client_rendered, "chars {}", m.choice.chosen.chars());
    assert!(!m.document.text.contains("priority lanes"));
}

#[test]
fn every_compiled_candidate_reads_every_fixture_without_failing() {
    for (name, html) in [("article", ARTICLE), ("docs", DOCS), ("forum", FORUM)] {
        for c in all_candidates() {
            if !c.candidate().compiled() {
                continue;
            }
            let r = c.extract(&PageInput { html, url: Some("https://ex.org/x") });
            let e = r.unwrap_or_else(|e| panic!("{} on {name}: {e}", c.candidate().as_str()));
            assert!(e.chars() > 200, "{} on {name}: {} chars", c.candidate().as_str(), e.chars());
        }
    }
}

/// Sleeps, then reads: a slow first candidate.
struct Slow(Candidate, Duration);
impl MainContent for Slow {
    fn candidate(&self) -> Candidate {
        self.0
    }
    fn extract(&self, _: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        std::thread::sleep(self.1);
        Ok(ex(self.0, 900, Some(0.9)))
    }
}

#[test]
fn a_first_candidate_past_its_budget_loses_to_the_racing_second() {
    let base = ex(Candidate::Baseline, 1_000, None);
    let s = Selector::new(
        vec![
            Box::new(Slow(Candidate::RsTrafilatura, Duration::from_millis(400))),
            Box::new(Counting(Candidate::RustTrafilatura, 900, Default::default())),
        ],
        SelectionPolicy::latency(),
    );
    let page = PageInput { html: "<p>x</p>", url: Some("https://blog.example.org/post/1") };
    let t = std::time::Instant::now();
    let c = s.choose_with(base.clone(), &page);
    assert!(t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());
    assert_eq!(c.chosen.candidate, Candidate::RustTrafilatura);
    assert_eq!(c.failures, vec![MainContentError::OverBudget(Candidate::RsTrafilatura)]);
    // A forum URL is exempt: the slow candidate is waited for.
    if Candidate::RsTrafilatura.compiled() {
        let forum = PageInput { html: "<p>x</p>", url: Some("https://stackoverflow.com/questions/1/how") };
        assert_eq!(url_page_type(forum.url.unwrap()), PageType::Forum);
        assert_eq!(s.choose_with(base, &forum).chosen.candidate, Candidate::RsTrafilatura);
    }
}

#[test]
fn the_default_policy_waits_for_an_ordinary_slow_page_and_caps_a_pathological_one() {
    let base = ex(Candidate::Baseline, 1_000, None);
    let page = PageInput { html: "<p>x</p>", url: Some("https://blog.example.org/post/1") };
    let rest = || Box::new(Counting(Candidate::RustTrafilatura, 900, Default::default()));
    // 300 ms is far past the latency cap but within the default one: quality wins.
    let s = Selector::new(
        vec![Box::new(Slow(Candidate::RsTrafilatura, Duration::from_millis(300))), rest()],
        SelectionPolicy::default(),
    );
    assert_eq!(s.choose_with(base.clone(), &page).chosen.candidate, Candidate::RsTrafilatura);
    // A page that would take seconds is cut at the cap.
    let s = Selector::new(
        vec![Box::new(Slow(Candidate::RsTrafilatura, Duration::from_secs(5))), rest()],
        SelectionPolicy::default(),
    );
    let t = std::time::Instant::now();
    let c = s.choose_with(base, &page);
    assert!(t.elapsed() < Duration::from_millis(2_500), "{:?}", t.elapsed());
    assert_eq!(c.chosen.candidate, Candidate::RustTrafilatura);
}

#[test]
fn lighten_drops_heavy_elements_and_keeps_json_ld() {
    let html = "<html><head><style>a{}</style><script>var x='<p>';</script>\
                <script type=\"application/ld+json\">{\"@type\":\"Article\"}</script></head>\
                <body><!-- note --><SVG><path/></svg><p>Kept text.</p><noscript>enable js</noscript></body></html>";
    let l = lighten(html);
    assert!(l.contains("Kept text.") && l.contains("ld+json") && l.contains("Article"), "{l}");
    for gone in ["a{}", "var x", "note", "path", "enable js"] {
        assert!(!l.contains(gone), "{gone} survived: {l}");
    }
    assert!(matches!(lighten("<p>plain</p>"), std::borrow::Cow::Borrowed(_)));
    assert_eq!(lighten("<p>a<script>unterminated"), "<p>a");
}
