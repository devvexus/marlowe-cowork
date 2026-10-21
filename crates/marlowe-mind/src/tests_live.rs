//! Live recall stages and the write tail. The observer must only observe: what is injected is the
//! same with the live display on and off. Tests that need the real models (CPU, no GPU) skip
//! loudly when `models/` is absent.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::bounded;

use crate::*;

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-mind-live-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn models_or_skip(test: &str) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
    let files = [
        dir.join(EMBEDDER_DIR).join("model.onnx"),
        dir.join(EMBEDDER_DIR).join("vocab.txt"),
        dir.join(RERANKER_DIR).join("model.onnx"),
        dir.join(RERANKER_DIR).join("tokenizer.json"),
    ];
    if files.iter().all(|f| f.is_file()) {
        Some(dir)
    } else {
        eprintln!("SKIPPED {test}: models are not in {} (git-ignored)", dir.display());
        None
    }
}

const FACTS: [&str; 6] = [
    "My favourite text editor is Helix.",
    "The staging cluster runs in Frankfurt.",
    "Our deploys go out every Friday afternoon.",
    "My daughter's piano lesson is on Tuesdays.",
    "The coffee machine on the third floor is broken.",
    "The quarterly budget review is owned by Priya.",
];

fn fill(mind: &Mind) {
    for f in FACTS {
        mind.remember(MemoryIn {
            text: f.into(),
            kind: Kind::Fact,
            scope: Scope::Global,
            source: Provenance::User { session: Some("s-a".into()) },
        })
        .unwrap();
    }
}

fn query(text: &str) -> InjectQuery {
    InjectQuery { session: "s-b".into(), workspace: None, text: text.into(), budget_tokens: 2_000, exclude: Vec::new() }
}

fn live(mind: &Mind, q: InjectQuery) -> (Injection, Vec<LiveEvent>) {
    let (tx, rx) = bounded(4096);
    let out = mind.inject_live(q, LiveSink::new(tx));
    (out, rx.try_iter().collect())
}

/// The stages that started, in order, with no repeats from progress events.
fn started(events: &[LiveEvent]) -> Vec<&'static str> {
    events.iter().filter(|e| e.kind == LiveKind::Start).map(|e| e.stage).collect()
}

#[test]
fn the_live_display_changes_nothing_the_mind_injects() {
    let Some(models) = models_or_skip("the_live_display_changes_nothing_the_mind_injects") else { return };
    let home = temp_home("identical");
    let mind = Mind::open_with(&home, MindConfig { models_dir: Some(models), ..MindConfig::default() }).unwrap();
    fill(&mind);
    let q = || query("Which text editor do I like best?");
    // The first call is cold: both models load, and the live one says so.
    let (watched, events) = live(&mind, q());
    let plain = mind.inject(q());

    assert_eq!(watched.abstained, plain.abstained);
    assert_eq!(watched.blocks, plain.blocks, "the same context blocks, in the same order");
    assert_eq!(watched.floor, plain.floor);
    let ids = |i: &Injection| i.recalled.iter().map(|h| (h.id.clone(), h.score.to_bits())).collect::<Vec<_>>();
    assert_eq!(ids(&watched), ids(&plain), "the same memories with the same scores");
    assert!(!watched.recalled.is_empty(), "the test needs a real injection: {watched:?}");
    assert!(watched.recalled[0].text.contains("Helix"));

    // Every stage streamed, in pipeline order, each closed, progress never past its total.
    let order = started(&events);
    for name in ["pool", "bm25", "dense", "features", "gate", "prune", "rerank", "assemble"] {
        assert!(order.contains(&name), "missing stage {name}: {order:?}");
    }
    let pos = |n: &str| order.iter().position(|s| *s == n).unwrap();
    assert!(pos("pool") < pos("bm25") && pos("bm25") < pos("dense") && pos("dense") < pos("gate") && pos("gate") < pos("rerank") && pos("rerank") < pos("assemble"));
    assert!(order.contains(&"embed") && pos("embed") < pos("pool"), "the query is embedded before retrieval: {order:?}");
    // The reranker only loads for a recall, so a first recall reports its cold load (the
    // embedder may already be up from the background queue).
    assert!(
        events.iter().any(|e| e.stage == "load" && e.detail.as_deref().is_some_and(|d| d.starts_with("reranker"))),
        "a cold run reports the reranker load: {order:?}"
    );
    let ends = events.iter().filter(|e| matches!(e.kind, LiveKind::End { .. })).count();
    assert_eq!(ends, order.len(), "every stage that started ended");
    for e in &events {
        if let LiveKind::Progress { done, total, .. } = e.kind {
            assert!(done <= total, "{e:?}");
        }
    }
    assert!(
        events.iter().any(|e| e.stage == "rerank" && e.kind == LiveKind::Start && matches!(e.detail.as_deref(), Some("cpu") | Some("gpu"))),
        "the rerank stage names its device"
    );
    // The summary the Recalled line carries has a time per stage, in the same order.
    let names: Vec<&str> = watched.stages.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"embed") && names.contains(&"rerank"), "{names:?}");
    mind.shutdown().unwrap();
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn an_abstained_recall_still_reports_the_stages_it_ran() {
    let Some(models) = models_or_skip("an_abstained_recall_still_reports_the_stages_it_ran") else { return };
    let home = temp_home("abstain");
    let mind = Mind::open_with(&home, MindConfig { models_dir: Some(models), ..MindConfig::default() }).unwrap();
    fill(&mind);
    let (got, events) = live(&mind, query("xylophone quantum lasagna zeppelin"));
    assert!(!events.is_empty(), "stages streamed even for a recall that finds nothing");
    assert!(!got.stages.is_empty(), "and are summarised: {got:?}");
    assert!(events.iter().all(|e| e.stage != ""));
    let starts = events.iter().filter(|e| e.kind == LiveKind::Start).count();
    let ends = events.iter().filter(|e| matches!(e.kind, LiveKind::End { .. })).count();
    assert_eq!(starts, ends, "no stage is left open when the recall ends early");
    mind.shutdown().unwrap();
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn without_models_the_live_call_degrades_like_the_plain_one() {
    let home = temp_home("degraded");
    let mind = Mind::open_with(&home, MindConfig { models_dir: Some(home.join("no-models-here")), ..MindConfig::default() }).unwrap();
    fill(&mind);
    let (watched, events) = live(&mind, query("Which text editor do I like best?"));
    let plain = mind.inject(query("Which text editor do I like best?"));
    assert_eq!(watched.abstained, plain.abstained);
    assert!(watched.abstained.is_some() && events.is_empty());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn the_write_tail_reports_ingest_embed_and_index() {
    let Some(models) = models_or_skip("the_write_tail_reports_ingest_embed_and_index") else { return };
    let home = temp_home("tail");
    let (tx, rx) = bounded(256);
    let cfg = MindConfig {
        models_dir: Some(models),
        on_event: Some(MindSink::new(move |e| {
            let _ = tx.try_send(e);
        })),
        ..MindConfig::default()
    };
    let mind = Mind::open_with(&home, cfg).unwrap();
    mind.ingest_turn(TurnIn {
        session: "s-a".into(),
        workspace: None,
        role: TurnRole::User,
        text: "My favourite text editor is Helix.".into(),
        at_ms: 1_000,
    })
    .unwrap();
    let mut seen = Vec::new();
    while let Ok(e) = rx.recv_timeout(Duration::from_secs(60)) {
        let done = e == MindEvent::Indexed;
        seen.push(e);
        if done {
            break;
        }
    }
    assert_eq!(seen.first(), Some(&MindEvent::Ingested), "{seen:?}");
    assert!(matches!(seen.get(1), Some(MindEvent::EmbedStart { queued }) if *queued >= 1), "{seen:?}");
    assert!(seen.iter().any(|e| matches!(e, MindEvent::EmbedProgress { done, total } if done <= total)), "{seen:?}");
    assert!(seen.iter().any(|e| matches!(e, MindEvent::EmbedDone { installed: 1, .. })), "{seen:?}");
    assert_eq!(seen.last(), Some(&MindEvent::Indexed), "{seen:?}");
    mind.shutdown().unwrap();
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_sink_set_after_open_hears_the_next_write() {
    let home = temp_home("late-sink");
    let mind = Mind::open_with(&home, MindConfig { models_dir: Some(home.join("none")), ..MindConfig::default() }).unwrap();
    let (tx, rx) = bounded(16);
    mind.set_event_sink(MindSink::new(move |e| {
        let _ = tx.try_send(e);
    }));
    fill(&mind);
    assert_eq!(rx.try_iter().filter(|e| *e == MindEvent::Ingested).count(), FACTS.len());
    let _ = fs::remove_dir_all(&home);
}
