//! Service tests against a temp MARLOWE_HOME. Tests that need the real models skip loudly when
//! `models/` is absent; the rest run with a deliberately empty models dir (degraded mode).

use std::fs;
use std::path::{Path, PathBuf};

use marlowe_contract::{Channel, Clock, IngestRequest, Origin as WireOrigin, PayloadKind, Speaker, TrustClass, Turn};
use marlowe_journal::{EventKind, Journal, OperatorCapability, Profile};
use marlowe_memory::{BeliefStore, ClaimWrite, MemoryEntry};

use crate::store::Boot;
use crate::*;
use crate::{actor, select, store};

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-mind-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The repo's models dir, or `None` (with a loud skip line) when the files are absent.
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
        eprintln!(
            "SKIPPED {test}: models are not in {} (git-ignored; fetch them with tools/fetch_model.py)",
            dir.display()
        );
        None
    }
}

fn user() -> Provenance {
    Provenance::User { session: None }
}

fn with_models(dir: &Path) -> MindConfig {
    MindConfig { models_dir: Some(dir.to_path_buf()), ..MindConfig::default() }
}

fn without_models(home: &Path) -> MindConfig {
    MindConfig { models_dir: Some(home.join("no-models-here")), ..MindConfig::default() }
}

fn user_fact(mind: &Mind, session: &str, text: &str) -> String {
    mind.remember(MemoryIn {
        text: text.into(),
        kind: Kind::Fact,
        scope: Scope::Global,
        source: Provenance::User { session: Some(session.into()) },
    })
    .expect("remember")
    .id
}

const FACTS: [&str; 6] = [
    "My favourite text editor is Helix.",
    "The staging cluster runs in Frankfurt.",
    "Our deploys go out every Friday afternoon.",
    "My daughter's piano lesson is on Tuesdays.",
    "The coffee machine on the third floor is broken.",
    "The quarterly budget review is owned by Priya.",
];

fn query(session: &str, text: &str) -> InjectQuery {
    InjectQuery { session: session.into(), workspace: None, text: text.into(), budget_tokens: 2_000, exclude: Vec::new() }
}

fn all_entries(store: &BeliefStore) -> Vec<MemoryEntry> {
    store.recall_candidates().into_iter().cloned().collect()
}

#[test]
fn a_fact_from_session_a_is_injected_in_session_b_after_reopening() {
    let Some(models) = models_or_skip("a_fact_from_session_a_is_injected_in_session_b_after_reopening") else { return };
    let home = temp_home("inject-across-sessions");
    {
        let mind = Mind::open_with(&home, with_models(&models)).unwrap();
        for f in FACTS {
            user_fact(&mind, "session-a", f);
        }
        // Assistant prose that answers the question outright must never be the one injected.
        mind.ingest_turn(TurnIn {
            session: "session-a".into(),
            workspace: None,
            role: TurnRole::Assistant,
            text: "Your favourite text editor is Helix, as you told me.".into(),
            at_ms: 1_000,
        })
        .unwrap();
    }
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    assert!(matches!(mind.boot_info().unwrap(), Boot::Snapshot { .. }), "a clean close leaves a snapshot");
    let got = mind.inject(query("session-b", "Which text editor do I like best?"));
    assert_eq!(got.abstained, None, "expected an injection, got {got:?}");
    assert!(got.recalled[0].text.contains("Helix"), "{got:?}");
    assert!(got.blocks[0].contains("Helix") && got.blocks[0].contains("you"), "{:?}", got.blocks);
    assert!(got.recalled.iter().all(|h| h.kind != MemoryKind::Episode), "assistant prose was injected: {got:?}");
    let stats = mind.stats().unwrap();
    assert_eq!(stats.degraded, None);
    assert_eq!(stats.models_loaded.len(), 2, "{stats:?}");
    assert!(stats.vectors >= FACTS.len() as u64, "{stats:?}");
    mind.shutdown().unwrap();
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_paraphrase_with_no_shared_words_is_recalled_through_the_dense_cue() {
    let Some(models) = models_or_skip("a_paraphrase_with_no_shared_words_is_recalled_through_the_dense_cue") else { return };
    let home = temp_home("paraphrase");
    let target = "I am allergic to peanuts.";
    let question = "Which foods should be avoided given this nut allergy?";
    let texts: Vec<&str> = FACTS.iter().copied().chain([target]).collect();
    let lexical = marlowe_memory::cue::lexical::score_texts(&texts, question);
    assert!(lexical.iter().all(|s| *s == 0.0), "the query must share no term with any memory: {lexical:?}");
    {
        let mind = Mind::open_with(&home, with_models(&models)).unwrap();
        for t in &texts {
            user_fact(&mind, "s1", t);
        }
        let hits = mind.recall(RecallQuery { text: question.into(), scope: None, limit: 3 }).unwrap();
        assert_eq!(hits.first().map(|h| h.text.as_str()), Some(target), "{hits:?}");
        let got = mind.inject(query("s2", question));
        if got.abstained.is_none() {
            assert_eq!(got.recalled[0].text, target, "{got:?}");
        }
        eprintln!("paraphrase injection: {:?} / {:?}", got.recalled.first().map(|h| &h.text), got.abstained);
    }
    // The same store without models finds nothing for this query: the hit above was dense.
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let hits = mind.recall(RecallQuery { text: question.into(), scope: None, limit: 3 }).unwrap();
    assert!(hits.is_empty(), "lexical-only recall should find nothing: {hits:?}");
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn the_embed_queue_coalesces_and_idle_models_unload() {
    let Some(models) = models_or_skip("the_embed_queue_coalesces_and_idle_models_unload") else { return };
    let home = temp_home("unload");
    let cfg = MindConfig { embed_queue: 2, model_idle: std::time::Duration::from_millis(300), ..with_models(&models) };
    let mind = Mind::open_with(&home, cfg).unwrap();
    assert!(mind.stats().unwrap().models_loaded.is_empty(), "models load lazily");
    let empty = mind.inject(query("s0", "anything at all"));
    assert!(empty.abstained.as_deref().is_some_and(|r| r.starts_with("no candidates")), "{empty:?}");
    assert!(mind.stats().unwrap().models_loaded.is_empty(), "an empty store loads no model");
    for f in FACTS {
        user_fact(&mind, "s1", f);
    }
    // Past the queue cap the background pass embeds everything missing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while mind.stats().unwrap().vectors < FACTS.len() as u64 {
        assert!(std::time::Instant::now() < deadline, "background embedding never finished");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    mind.inject(query("s2", "Which text editor do I like best?"));
    assert_eq!(mind.stats().unwrap().models_loaded.len(), 2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !mind.stats().unwrap().models_loaded.is_empty() {
        assert!(std::time::Instant::now() < deadline, "idle models were not unloaded");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(mind.stats().unwrap().vectors, FACTS.len() as u64, "vectors outlive the models");
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn degraded_mode_is_reported_abstains_and_still_recalls() {
    let home = temp_home("degraded");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let stats = mind.stats().unwrap();
    let reason = stats.degraded.expect("degraded must be reported");
    assert!(reason.contains("missing") && reason.contains("auto-injection is off"), "{reason}");
    assert!(stats.models_loaded.is_empty());

    let id = user_fact(&mind, "s1", "The staging cluster runs in Frankfurt.");
    user_fact(&mind, "s1", "Our deploys go out every Friday afternoon.");
    let got = mind.inject(query("s2", "where does the staging cluster run?"));
    assert!(got.blocks.is_empty() && got.recalled.is_empty());
    assert!(got.abstained.as_deref().is_some_and(|r| r.starts_with("degraded")), "{got:?}");

    let hits = mind.recall(RecallQuery { text: "staging cluster".into(), scope: None, limit: 5 }).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let h = &hits[0];
    assert_eq!((h.id.as_str(), h.kind, h.trust), (id.as_str(), MemoryKind::Fact, Trust::User));
    assert_eq!(h.scope, Scope::Global);
    assert_eq!(h.provenance, "you (session s1)");

    // Forget works without models too, and survives a reopen.
    mind.forget(&id, user()).unwrap();
    assert!(mind.recall(RecallQuery { text: "staging cluster".into(), scope: None, limit: 5 }).unwrap().is_empty());
    drop(mind);
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    assert!(mind.recall(RecallQuery { text: "staging cluster".into(), scope: None, limit: 5 }).unwrap().is_empty());
    assert!(matches!(mind.forget("m-nope", user()), Err(MindError::NotFound(_))));
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn forget_removes_a_memory_from_injection() {
    let Some(models) = models_or_skip("forget_removes_a_memory_from_injection") else { return };
    let home = temp_home("forget");
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    let mut ids = Vec::new();
    for f in FACTS {
        ids.push(user_fact(&mind, "s1", f));
    }
    let q = query("s2", "Which text editor do I like best?");
    let before = mind.inject(q.clone());
    assert_eq!(before.recalled.first().map(|h| h.id.as_str()), Some(ids[0].as_str()), "{before:?}");
    mind.forget(&ids[0], user()).unwrap();
    let after = mind.inject(q.clone());
    assert!(after.recalled.iter().all(|h| h.id != ids[0]), "a forgotten memory was injected: {after:?}");
    assert!(after.blocks.iter().all(|b| !b.contains("Helix")), "{after:?}");
    drop(mind);
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    let again = mind.inject(q);
    assert!(again.recalled.iter().all(|h| h.id != ids[0]), "forget did not survive a reopen: {again:?}");
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn trust_sets_maturation_and_corrections_cannot_lower_authority() {
    let home = temp_home("trust");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let user = mind
        .remember(MemoryIn {
            text: "Deploy freeze starts on the 20th.".into(),
            kind: Kind::Decision { reason: "the auditors arrive".into() },
            scope: Scope::Global,
            source: Provenance::User { session: None },
        })
        .unwrap();
    assert_eq!((user.trust, user.matures_at_ms), (Trust::User, None), "user memories are instant");

    let agent = mind
        .remember(MemoryIn {
            text: "The CI runner pool has eight machines.".into(),
            kind: Kind::Result,
            scope: Scope::Global,
            source: Provenance::Agent { session: "s1".into(), turn: 3, tainted: false },
        })
        .unwrap();
    assert_eq!(agent.trust, Trust::Inferred);
    // M2.6: a deliberate write from an untainted session is instant; a tainted one matures.
    assert_eq!(agent.matures_at_ms, None, "{agent:?}");
    let now = actor::now_ms();
    let window = marlowe_memory::MATURATION_WINDOW_MS;
    let web = mind
        .remember(MemoryIn {
            text: "A blog says the runner image is deprecated.".into(),
            kind: Kind::Note,
            scope: Scope::Global,
            source: Provenance::Agent { session: "s1".into(), turn: 4, tainted: true },
        })
        .unwrap();
    assert_eq!(web.trust, Trust::Untrusted);
    assert!(web.matures_at_ms.is_some_and(|t| t > now + window - 60_000 && t <= now + window), "{web:?}");

    let refused = mind.correct(&user.id, "Deploy freeze starts on the 25th.", Provenance::Agent {
        session: "s1".into(),
        turn: 5,
        tainted: false,
    });
    assert!(matches!(refused, Err(MindError::Rejected(_))), "{refused:?}");
    let fixed = mind.correct(&user.id, "Deploy freeze starts on the 25th.", Provenance::User { session: None }).unwrap();
    let hits = mind.recall(RecallQuery { text: "deploy freeze".into(), scope: None, limit: 5 }).unwrap();
    assert_eq!(hits.len(), 1, "the superseded memory leaves recall: {hits:?}");
    assert_eq!(hits[0].id, fixed.id);
    assert_eq!(hits[0].kind, MemoryKind::Decision, "a correction keeps the kind");
    assert!(hits[0].provenance.contains(&format!("corrects {}", user.id)), "{:?}", hits[0]);

    // Same text again is the same memory.
    let again = mind
        .remember(MemoryIn {
            text: "The CI runner pool has eight machines.".into(),
            kind: Kind::Result,
            scope: Scope::Global,
            source: Provenance::Agent { session: "s1".into(), turn: 6, tainted: false },
        })
        .unwrap();
    assert_eq!(again.id, agent.id);
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn explicit_project_scope_filters_recall_and_turns_are_global() {
    let home = temp_home("scope");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    mind.ingest_turn(TurnIn {
        session: "s1".into(),
        workspace: Some(r"C:\work\alpha".into()),
        role: TurnRole::User,
        text: "alpha uses the orchid build system".into(),
        at_ms: 5,
    })
    .unwrap();
    mind.remember(MemoryIn {
        text: "orchid builds need the beta toolchain".into(),
        kind: Kind::Procedure,
        scope: Scope::Project("C:/work/beta/".into()),
        source: Provenance::User { session: None },
    })
    .unwrap();
    user_fact(&mind, "s1", "orchid is pronounced like the flower");
    let search = |scope: Option<Scope>| {
        let mut texts: Vec<String> = mind
            .recall(RecallQuery { text: "orchid".into(), scope, limit: 10 })
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        texts.sort();
        texts
    };
    assert_eq!(search(None).len(), 3);
    // The turn is Global: what was said in alpha is visible from everywhere.
    let global = vec!["alpha uses the orchid build system", "orchid is pronounced like the flower"];
    assert_eq!(search(Some(Scope::Global)), global);
    assert_eq!(search(Some(Scope::Project("C:/work/alpha/".into()))), global);
    // An explicit Project memory stays inside its project.
    let beta = search(Some(Scope::Project(r"C:\work\beta".into())));
    assert_eq!(
        beta,
        vec!["alpha uses the orchid build system", "orchid builds need the beta toolchain", "orchid is pronounced like the flower"]
    );
    let hit = mind.recall(RecallQuery { text: "alpha".into(), scope: None, limit: 1 }).unwrap().remove(0);
    assert_eq!((hit.kind, hit.trust), (MemoryKind::Note, Trust::User));
    assert_eq!(hit.scope, Scope::Global);
    assert_eq!(hit.occurred_at_ms, 5);
    // The session's workspace is remembered for the boost and survives a reopen.
    drop(mind);
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    mind.write_episode("s1", "Talked about orchid.", 9, false).unwrap();
    assert_eq!(mind.episodes(0..10).unwrap()[0].workspace.as_deref(), Some(r"C:\work\alpha"));
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn episodes_carry_the_sessions_worst_trust_and_are_listed_by_time() {
    let home = temp_home("episodes");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    for (role, text) in [(TurnRole::User, "let's fix the flaky test"), (TurnRole::Assistant, "I found a race in setup")] {
        mind.ingest_turn(TurnIn { session: "s9".into(), workspace: Some("C:/w".into()), role, text: text.into(), at_ms: 10 })
            .unwrap();
    }
    let id = mind.write_episode("s9", "Fixed the flaky test: a race in setup.", 2_000, false).unwrap();
    mind.write_episode("s10", "Planned the release.", 1_000, false).unwrap();
    let eps = mind.episodes(0..5_000).unwrap();
    assert_eq!(eps.iter().map(|e| e.at_ms).collect::<Vec<_>>(), vec![1_000, 2_000]);
    assert_eq!(eps[1].id, id);
    assert_eq!(eps[1].workspace.as_deref(), Some("C:/w"));
    assert!(mind.episodes(1_500..1_600).unwrap().is_empty());
    let hit = mind.recall(RecallQuery { text: "flaky race".into(), scope: None, limit: 5 }).unwrap();
    let ep = hit.iter().find(|h| h.id == id).expect("episode is recallable");
    assert_eq!((ep.kind, ep.trust), (MemoryKind::Episode, Trust::Inferred));
    let stats = mind.stats().unwrap();
    assert_eq!((stats.beliefs, stats.episodes), (4, 2));
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn snapshot_boot_equals_a_full_derive() {
    let home = temp_home("snapshot");
    let profile = profile_dir(&home);
    let (mid_seq, first) = {
        let mind = Mind::open_with(&home, without_models(&home)).unwrap();
        assert_eq!(mind.boot_info().unwrap(), Boot::Derived);
        let a = user_fact(&mind, "s1", "first fact before the snapshot");
        let b = user_fact(&mind, "s1", "second fact before the snapshot");
        mind.ingest_turn(TurnIn { session: "s1".into(), workspace: None, role: TurnRole::Assistant, text: "noted".into(), at_ms: 3 })
            .unwrap();
        let mid = mind.snapshot().unwrap();
        assert_eq!(mid.beliefs, 3);
        fs::copy(&mid.path, home.join("mid.snapshot")).unwrap();
        // The tail touches beliefs from before the watermark.
        mind.forget(&a, user()).unwrap();
        mind.correct(&b, "second fact, corrected", Provenance::User { session: None }).unwrap();
        user_fact(&mind, "s2", "a fact after the snapshot");
        mind.write_episode("s1", "summary", 4, false).unwrap();
        (mid.journal_seq, a)
    };
    // Put the mid-run snapshot back so boot has to fold a tail.
    fs::copy(home.join("mid.snapshot"), profile.join(store::SNAPSHOT_FILE)).unwrap();
    {
        let p = Profile::open(&profile).unwrap();
        let journal = Journal::open(&p).unwrap();
        let (booted, how) = store::boot(&profile, p.key(), &journal, p.manifest().derivation_version).unwrap();
        assert!(matches!(how, Boot::Snapshot { watermark, tail } if watermark == mid_seq && tail >= 4), "{how:?}");
        let derived = BeliefStore::derive(&journal, p.manifest().derivation_version).unwrap();
        assert_eq!(all_entries(&booted), all_entries(&derived));
        assert_eq!(booted.get(&first).unwrap().fidelity, marlowe_contract::Fidelity::Tombstone);
    }
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    assert!(matches!(mind.boot_info().unwrap(), Boot::Snapshot { watermark, .. } if watermark == mid_seq));
    assert_eq!(mind.stats().unwrap().beliefs, 4, "second(corrected), assistant turn, post-snapshot fact, episode");
    drop(mind);

    // A snapshot from another profile is ignored, not trusted.
    let other = temp_home("snapshot-other");
    drop(Mind::open_with(&other, without_models(&other)).unwrap());
    fs::copy(home.join("mid.snapshot"), profile_dir(&other).join(store::SNAPSHOT_FILE)).unwrap();
    let mind = Mind::open_with(&other, without_models(&other)).unwrap();
    assert_eq!(mind.boot_info().unwrap(), Boot::Derived);
    assert_eq!(mind.stats().unwrap().beliefs, 0);
    drop(mind);
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&other);
}

// ---- trust, taint and persisted vectors ----------------------------------------------------

fn agent(tainted: bool) -> Provenance {
    Provenance::Agent { session: "s1".into(), turn: 1, tainted }
}

/// Writes an External (untrusted) belief dated past the maturation window straight into the
/// closed profile, so it is an injection candidate when the service reopens.
fn plant_matured_untrusted(home: &Path, text: &str) -> String {
    use marlowe_memory::{Author, BeliefKind, BeliefWrite, MaturationPolicy, Tags};
    let p = Profile::open(&profile_dir(home)).unwrap();
    let mut journal = Journal::open(&p).unwrap();
    let mut beliefs = BeliefStore::derive(&journal, p.manifest().derivation_version).unwrap();
    let old = actor::now_ms() - marlowe_memory::MATURATION_WINDOW_MS - 3_600_000;
    let w = BeliefWrite {
        session_id: "web",
        turn_id: "page",
        run_id: None,
        text,
        author: Author::External,
        derived_from: &[],
        occurred_at_ms: old,
        tags: Tags { kind: Some(BeliefKind::Fact), ..Tags::default() },
        maturation: MaturationPolicy::ByTrust,
    };
    marlowe_memory::write_belief(&mut journal, &mut beliefs, Clock::new(old), &w).unwrap().unwrap().id
}

fn wait_for_vectors(mind: &Mind, n: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while mind.stats().unwrap().vectors < n as u64 {
        assert!(std::time::Instant::now() < deadline, "background embedding never finished");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Same memories, same scores to the bit, same blocks.
fn assert_same_injection(a: &Injection, b: &Injection) {
    let key = |i: &Injection| i.recalled.iter().map(|h| (h.id.clone(), h.score.to_bits())).collect::<Vec<_>>();
    assert_eq!(key(a), key(b), "{a:?}\n{b:?}");
    assert_eq!(a.blocks, b.blocks);
    assert_eq!(a.floor, b.floor);
}

#[test]
fn forget_needs_at_least_the_memorys_trust() {
    let home = temp_home("forget-authority");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let said = user_fact(&mind, "s1", "The staging cluster runs in Frankfurt.");
    let inferred = mind
        .remember(MemoryIn {
            text: "The CI pool has eight runners.".into(),
            kind: Kind::Fact,
            scope: Scope::Global,
            source: agent(false),
        })
        .unwrap()
        .id;
    let refused = mind.forget(&said, agent(false));
    assert!(matches!(refused, Err(MindError::Rejected(_))), "the conductor cannot erase what the user said: {refused:?}");
    let refused = mind.forget(&inferred, agent(true));
    assert!(matches!(refused, Err(MindError::Rejected(_))), "a tainted run cannot erase an inferred memory: {refused:?}");
    assert_eq!(mind.get(&said).unwrap().map(|h| h.trust), Some(Trust::User));
    assert!(mind.get(&inferred).unwrap().is_some());
    mind.forget(&inferred, agent(false)).unwrap();
    mind.forget(&said, user()).unwrap();
    assert!(mind.get(&said).unwrap().is_none() && mind.get(&inferred).unwrap().is_none());
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_tainted_sessions_episode_is_untrusted() {
    let home = temp_home("episode-taint");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let turn = TurnIn { session: "s1".into(), workspace: None, role: TurnRole::User, text: "read the vendor docs".into(), at_ms: 10 };
    mind.ingest_turn(turn).unwrap();
    let tainted = mind.write_episode("s1", "Read the vendor docs on API keys.", 20, true).unwrap();
    let clean = mind.write_episode("s2", "Planned the vendor migration.", 30, false).unwrap();
    let hits = mind.recall(RecallQuery { text: "vendor".into(), scope: None, limit: 10 }).unwrap();
    let trust = |id: &str| hits.iter().find(|h| h.id == id).map(|h| h.trust);
    assert_eq!(trust(&tainted), Some(Trust::Untrusted), "{hits:?}");
    assert_eq!(trust(&clean), Some(Trust::Inferred), "{hits:?}");
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn an_injected_untrusted_memory_reports_an_untrusted_floor() {
    let Some(models) = models_or_skip("an_injected_untrusted_memory_reports_an_untrusted_floor") else { return };
    let home = temp_home("inject-floor");
    {
        let mind = Mind::open_with(&home, with_models(&models)).unwrap();
        for f in &FACTS[1..] {
            user_fact(&mind, "s1", f);
        }
        mind.shutdown().unwrap();
    }
    let planted = plant_matured_untrusted(&home, FACTS[0]);
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    let got = mind.inject(query("s2", "Which text editor do I like best?"));
    assert_eq!(got.abstained, None, "{got:?}");
    assert!(got.recalled.iter().any(|h| h.id == planted && h.trust == Trust::Untrusted), "{got:?}");
    assert_eq!(got.floor, Some(Trust::Untrusted));
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn vectors_persist_and_a_reopened_store_injects_exactly_as_after_a_rebuild() {
    let Some(models) = models_or_skip("vectors_persist_and_a_reopened_store_injects_exactly_as_after_a_rebuild") else {
        return;
    };
    let home = temp_home("vectors");
    let filler: Vec<String> = (0..80)
        .map(|i| format!("Build note {i}: shard {} of the nightly dataset lives on volume {}.", i * 7 % 13, i % 5))
        .collect();
    let total = FACTS.len() + filler.len();
    {
        let mind = Mind::open_with(&home, with_models(&models)).unwrap();
        for f in FACTS.iter().copied().chain(filler.iter().map(String::as_str)) {
            user_fact(&mind, "s1", f);
        }
        wait_for_vectors(&mind, total);
    }
    let q = query("s2", "Which text editor do I like best?");
    let from_file = {
        let mind = Mind::open_with(&home, with_models(&models)).unwrap();
        assert_eq!(mind.stats().unwrap().vectors, total as u64, "vectors load with the store");
        let got = mind.inject(q.clone());
        assert_eq!(got.abstained, None, "{got:?}");
        got
    };
    // Without the file every vector is missing at boot: injection must not score the gap as 0.
    fs::remove_file(profile_dir(&home).join(crate::vecfile::VECTORS_FILE)).unwrap();
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    let early = mind.inject(q.clone());
    match early.abstained.as_deref() {
        Some(r) => assert!(r.starts_with("memory index warming"), "{early:?}"),
        None => assert_same_injection(&early, &from_file),
    }
    wait_for_vectors(&mind, total);
    assert_same_injection(&mind.inject(q), &from_file);
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

// ---- legacy import ------------------------------------------------------------------------

const T_OLD: i64 = 1_700_000_000_000;

fn old_turn(id: &str, text: &str, channel: Channel) -> Turn {
    Turn {
        turn_id: id.into(),
        speaker: Speaker::User,
        text: text.into(),
        occurred_at_ms: T_OLD,
        origin: WireOrigin { channel, actor: "user:primary".into(), r#ref: None },
    }
}

/// Builds an old-harness profile with the old write paths: user and web turns, a claim, a
/// correction of it, and a forgotten memory.
fn synthetic_old_profile(dir: &Path) -> Vec<String> {
    let profile = Profile::init(dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut beliefs = BeliefStore::default();
    let request = IngestRequest {
        contract_version: "1.0".into(),
        clock: Clock::new(T_OLD + 5),
        session_id: "old-session".into(),
        turns: vec![
            old_turn("t1", "my sister Ottilie lives in Porto", Channel::Terminal),
            old_turn("t2", "a forum post claims zanzibar ports are closed", Channel::Web),
            old_turn("t3", "the secret wifi password is hunter2", Channel::Terminal),
        ],
    };
    let out = marlowe_memory::ingest(&mut journal, &mut beliefs, &request).unwrap();
    let ids: Vec<String> = out.written.iter().flat_map(|w| w.memory_ids.clone()).collect();
    let claim = |text: &'static str| ClaimWrite {
        session_id: "old-session",
        run_id: "run-1",
        text,
        payload_kind: PayloadKind::Fact,
        derived_from: &[],
        run_floor: TrustClass::UserAsserted,
    };
    let clock = Clock::new(T_OLD + 10);
    let first = marlowe_memory::remember_claim(&mut journal, &mut beliefs, clock, &claim("the build takes nine minutes"))
        .unwrap()
        .unwrap();
    marlowe_memory::correct_claim(&mut journal, &mut beliefs, clock, &claim("the build takes twelve minutes"), &first.id)
        .unwrap()
        .unwrap();
    marlowe_memory::forget_claim(&mut journal, &mut beliefs, clock, "old-session", "run-1", &ids[2]).unwrap().unwrap();
    ids
}

fn digest_dir(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let bytes = fs::read(e.path()).unwrap_or_default();
            (e.file_name().to_string_lossy().into_owned(), blake3::hash(&bytes).to_hex().to_string())
        })
        .collect();
    out.sort();
    out
}

#[test]
fn import_legacy_is_idempotent_and_keeps_time_trust_and_provenance() {
    let old = temp_home("legacy-old").join("default-profile");
    synthetic_old_profile(&old);
    let before = digest_dir(&old);
    let home = temp_home("legacy-new");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();

    let status = mind.legacy_status(Some(&old)).unwrap().expect("an old profile is there");
    assert_eq!((status.approx_entries, status.already_imported), (5, false));

    let mut progress = Vec::new();
    let report = mind.import_legacy(&old, &mut |p| progress.push(p)).unwrap();
    assert_eq!(report.read, 7, "5 written + 1 superseded + 1 tombstoned: {report:?}");
    assert_eq!((report.imported, report.skipped_duplicates), (4, 0), "{report:?}");
    assert_eq!((report.tombstoned, report.superseded), (1, 1), "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(progress.last().copied(), Some(ImportProgress { read: 7, imported: 4 }));

    let find = |q: &str| mind.recall(RecallQuery { text: q.into(), scope: None, limit: 5 }).unwrap();
    let sister = find("Ottilie Porto").remove(0);
    assert_eq!(sister.text, "my sister Ottilie lives in Porto");
    assert_eq!((sister.trust, sister.occurred_at_ms), (Trust::User, T_OLD));
    assert_eq!(sister.provenance, LEGACY_PROVENANCE);
    assert_eq!(find("zanzibar").remove(0).trust, Trust::Untrusted);
    let build = find("build minutes");
    assert_eq!(build.len(), 1, "the superseded claim stays superseded: {build:?}");
    assert_eq!((build[0].text.as_str(), build[0].trust), ("the build takes twelve minutes", Trust::Inferred));
    assert!(find("hunter2 wifi password").is_empty(), "a forgotten memory came back");

    let again = mind.import_legacy(&old, &mut |_| {}).unwrap();
    assert_eq!((again.imported, again.skipped_duplicates, again.superseded), (0, 4, 0), "{again:?}");
    assert_eq!(mind.stats().unwrap().beliefs, 3);
    assert!(mind.legacy_status(Some(&old)).unwrap().unwrap().already_imported);
    mind.shutdown().unwrap();

    assert_eq!(digest_dir(&old), before, "the old profile must never be written");
    // The forgotten text never reached the new journal.
    let p = Profile::open(profile_dir(&home)).unwrap();
    let journal = Journal::open(&p).unwrap();
    let events = journal.replay(&OperatorCapability::for_operator_or_audit(), None).unwrap();
    assert!(events.iter().all(|(_, _, v)| !v.to_string().contains("hunter2")));
    assert_eq!(events.iter().filter(|(_, k, _)| *k == EventKind::MemoryWritten).count(), 4);
    drop(journal);
    drop(p);
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(old.parent().unwrap());
}

#[test]
fn import_refuses_a_missing_or_tampered_profile() {
    let home = temp_home("legacy-bad");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let nowhere = home.join("nowhere");
    assert!(matches!(mind.import_legacy(&nowhere, &mut |_| {}), Err(MindError::NoLegacyProfile(_))));
    assert_eq!(mind.legacy_status(Some(&nowhere)).unwrap(), None);

    let old = home.join("old");
    synthetic_old_profile(&old);
    // A different key cannot verify the chain.
    let other = home.join("other");
    drop(Profile::init(&other).unwrap());
    fs::copy(other.join("profile.key"), old.join("profile.key")).unwrap();
    let err = mind.import_legacy(&old, &mut |_| {}).unwrap_err();
    assert!(matches!(err, MindError::LegacyChain(_)), "{err:?}");
    assert_eq!(mind.stats().unwrap().beliefs, 0);
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_closed_service_reports_closed() {
    let home = temp_home("closed");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    let other = mind.clone();
    mind.shutdown().unwrap();
    assert!(matches!(other.stats(), Err(MindError::Closed)));
    assert!(other.inject(query("s", "anything")).abstained.is_some());
    // The lock is released: the profile opens again.
    drop(Mind::open_with(&home, without_models(&home)).unwrap());
    drop((mind, other));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn civil_dates_are_utc_days() {
    assert_eq!(select::civil_date(0), "1970-01-01");
    assert_eq!(select::civil_date(T_OLD), "2023-11-14");
    assert_eq!(select::civil_date(951_782_400_000), "2000-02-29");
}

// ---- capture policy and the workspace boost -------------------------------------------------

fn turn(session: &str, workspace: Option<&str>, role: TurnRole, text: &str) -> TurnIn {
    TurnIn { session: session.into(), workspace: workspace.map(str::to_string), role, text: text.into(), at_ms: 7 }
}

#[test]
fn user_questions_and_requests_are_episodes_and_statements_are_notes() {
    let home = temp_home("capture");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    for t in ["My cat Biscuit is twelve years old.", "How old is Biscuit?", "summarise the Biscuit vet notes"] {
        mind.ingest_turn(turn("s1", None, TurnRole::User, t)).unwrap();
    }
    let hits = mind.recall(RecallQuery { text: "Biscuit".into(), scope: None, limit: 10 }).unwrap();
    let kind = |t: &str| hits.iter().find(|h| h.text == t).map(|h| (h.kind, h.trust));
    assert_eq!(kind("My cat Biscuit is twelve years old."), Some((MemoryKind::Note, Trust::User)));
    assert_eq!(kind("How old is Biscuit?"), Some((MemoryKind::Episode, Trust::User)), "{hits:?}");
    assert_eq!(kind("summarise the Biscuit vet notes"), Some((MemoryKind::Episode, Trust::User)), "{hits:?}");
    // A request is not a session summary.
    assert_eq!(mind.stats().unwrap().episodes, 0);
    assert!(mind.episodes(0..100).unwrap().is_empty());
    // Remembering the same words as a fact is a new memory, not the stored question.
    let fact = mind
        .remember(MemoryIn { text: "How old is Biscuit?".into(), kind: Kind::Fact, scope: Scope::Global, source: user() })
        .unwrap();
    assert_eq!(mind.get(&fact.id).unwrap().map(|h| h.kind), Some(MemoryKind::Fact));
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn the_boost_keeps_rank_one_and_prefers_same_project_memories_for_later_slots() {
    use crate::select::{fill_slots, Slot, WORKSPACE_BOOST};
    fn slot(id: &str, rerank: f32, same_project: bool) -> Slot<'_> {
        Slot { id, text: "a short memory", rerank, score: 0.0, margin: 0.0, same_project }
    }
    let ids = |v: Vec<(String, f32)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    let first = ("m-top", "the top memory", 1.0);
    // Without a same-project candidate the order is the core's: rerank logit, then id.
    let plain = vec![slot("m-b", 2.0, false), slot("m-c", 3.0, false), slot("m-a", 2.0, false)];
    assert_eq!(ids(fill_slots(first, plain, 1_000)), vec!["m-top", "m-c", "m-a"]);
    // Within the boost, a same-project memory overtakes; beyond it, it does not.
    let near = vec![slot("m-c", 3.0, false), slot("m-b", 2.0, false), slot("m-p", 3.0 - WORKSPACE_BOOST / 2.0, true)];
    assert_eq!(ids(fill_slots(first, near, 1_000)), vec!["m-top", "m-p", "m-c"]);
    let far = vec![slot("m-c", 3.0, false), slot("m-b", 2.9, false), slot("m-p", 3.0 - WORKSPACE_BOOST * 2.0, true)];
    assert_eq!(ids(fill_slots(first, far, 1_000)), vec!["m-top", "m-c", "m-b"]);
    // The budget cuts as the core's does: rank 1 always, then stop at the first that misses.
    let tight = vec![slot("m-c", 3.0, false)];
    let one = marlowe_memory::retrieve::estimate_tokens(first.1);
    assert_eq!(ids(fill_slots(first, tight, one)), vec!["m-top"]);
}

#[test]
fn questions_are_never_injected_and_turns_cross_workspaces_but_project_memories_do_not() {
    let Some(models) = models_or_skip("questions_are_never_injected_and_turns_cross_workspaces_but_project_memories_do_not")
    else {
        return;
    };
    let home = temp_home("capture-inject");
    let (alpha, beta) = (r"C:\work\alpha", r"C:\work\beta");
    let mind = Mind::open_with(&home, with_models(&models)).unwrap();
    for f in &FACTS[1..] {
        mind.ingest_turn(turn("sa", Some(alpha), TurnRole::User, f)).unwrap();
    }
    // Word for word the query below: without the capture policy it would win rank 1.
    mind.ingest_turn(turn("sa", Some(alpha), TurnRole::User, "Which text editor do I like best?")).unwrap();
    mind.remember(MemoryIn {
        text: FACTS[0].into(),
        kind: Kind::Preference,
        scope: Scope::Project(alpha.into()),
        source: Provenance::User { session: Some("sa".into()) },
    })
    .unwrap();
    let ask = |ws: &str| {
        mind.inject(InjectQuery {
            session: "sq".into(),
            workspace: Some(ws.into()),
            text: "Which text editor do I like best?".into(),
            budget_tokens: 2_000,
            exclude: Vec::new(),
        })
    };
    let home_ws = ask(alpha);
    assert_eq!(home_ws.abstained, None, "{home_ws:?}");
    assert_eq!(home_ws.recalled[0].text, FACTS[0], "{home_ws:?}");
    assert!(home_ws.recalled.iter().all(|h| h.kind != MemoryKind::Episode), "a question was injected: {home_ws:?}");
    let away = ask(beta);
    assert!(away.recalled.iter().all(|h| h.text != FACTS[0]), "another project's memory leaked: {away:?}");
    assert!(away.recalled.iter().all(|h| h.kind != MemoryKind::Episode), "a question was injected: {away:?}");

    // A statement made in alpha reaches beta: turns are Global.
    mind.ingest_turn(turn("sa", Some(alpha), TurnRole::User, "My favourite text editor is Kakoune.")).unwrap();
    let away = ask(beta);
    assert_eq!(away.abstained, None, "{away:?}");
    assert!(away.recalled[0].text.contains("Kakoune"), "{away:?}");
    drop(mind);
    let _ = fs::remove_dir_all(&home);
}

// ---- legacy conversation import -------------------------------------------------------------

/// One `state.volatile[]` block as the old harness wrote it.
fn block(source: &str, trust: &str, text: &str) -> serde_json::Value {
    serde_json::json!({ "source": source, "text": text, "tokens": text.len() / 4, "trust": trust, "wire": null })
}

/// The old profile of [`synthetic_old_profile`] plus `checkpointed` events carrying the
/// conversation the way the old harness did: cumulative history, tool results, skill banners,
/// machine payloads and step-only checkpoints.
fn synthetic_old_profile_with_conversation(dir: &Path) {
    use marlowe_journal::{Actor as JActor, AppendRequest, TraceId};
    synthetic_old_profile(dir);
    let profile = Profile::open(dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let banner = "Possibly relevant here:\n- release-notes: Produce release notes.\nLoad one with `skill`.";
    let first = vec![
        block("history", "user_asserted", "My cat Biscuit is twelve years old."),
        block("history", "user_asserted", "What time is it?"),
        block("history", "agent_inferred", "It is noon in Porto."),
        block("tool_results", "agent_observed", "README.md\nCargo.toml"),
        block("skills", "user_asserted", banner),
        block("history", "agent_inferred", r#"[{"path":"budget.rs","summary":"Enforces budget limits"}]"#),
        block("history", "agent_observed", "[spawned] tools: read · budget: 4000 tokens · returns: what you found"),
        block("history", "agent_inferred", ""),
        block("history", "user_asserted", "   "),
    ];
    let mut second = first.clone();
    second.push(block("history", "user_asserted", "I prefer dark roast coffee."));
    let third = vec![
        block("history", "user_asserted", "What time is it?"),
        block("history", "agent_observed", "[output contract] field `findings` is 2644 characters and the contract caps it at 2000"),
        block("history", "user_asserted", "summarise the README"),
    ];
    let events = [
        ("old-a", T_OLD + 50, serde_json::json!({ "step": 1 })),
        ("old-a", T_OLD + 100, serde_json::json!({ "session": "old-a", "state": { "session": "old-a", "volatile": first } })),
        ("old-a", T_OLD + 200, serde_json::json!({ "session": "old-a", "state": { "session": "old-a", "volatile": second } })),
        ("old-b", T_OLD + 300, serde_json::json!({ "session": "old-b", "state": { "session": "old-b", "volatile": third } })),
    ];
    for (session, ts, payload) in events {
        journal
            .append(
                Clock::new(ts),
                AppendRequest {
                    trace_id: TraceId::new_v5(&TraceId::NAMESPACE_OID, session.as_bytes()),
                    session_id: Some(session.into()),
                    run_id: Some("run-1".into()),
                    actor: JActor::Harness,
                    kind: EventKind::Checkpointed,
                    payload,
                },
            )
            .unwrap();
    }
}

#[test]
fn import_legacy_brings_the_old_conversation_over_as_turns() {
    let old = temp_home("legacy-talk-old").join("default-profile");
    synthetic_old_profile_with_conversation(&old);
    let before = digest_dir(&old);
    let home = temp_home("legacy-talk-new");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();

    // A marker from an import that predates the conversation import does not count.
    let v1 = r#"{"source":"x","completed_at_ms":1,"report":{"read":7,"imported":4,"skipped_duplicates":0,"tombstoned":1,"superseded":1}}"#;
    fs::write(profile_dir(&home).join("legacy-import.json"), v1).unwrap();
    let status = mind.legacy_status(Some(&old)).unwrap().unwrap();
    assert_eq!((status.approx_entries, status.already_imported), (5 + 5, false), "{status:?}");

    let mut progress = Vec::new();
    let report = mind.import_legacy(&old, &mut |p| progress.push(p)).unwrap();
    assert_eq!(report.conversation_turns_read, 8, "distinct non-empty history texts: {report:?}");
    assert_eq!(report.conversation_skipped, 3, "JSON tool results and two harness status lines: {report:?}");
    assert_eq!(report.conversation_turns_imported, 5, "{report:?}");
    assert_eq!(
        (report.conversation_user_turns, report.conversation_user_requests, report.conversation_assistant_turns),
        (4, 2, 1),
        "{report:?}"
    );
    assert_eq!((report.read, report.imported, report.skipped_duplicates), (7 + 8, 4 + 5, 0), "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(progress.last().copied(), Some(ImportProgress { read: 15, imported: 9 }));

    let find = |q: &str| mind.recall(RecallQuery { text: q.into(), scope: None, limit: 10 }).unwrap();
    let cat = find("Biscuit twelve").remove(0);
    assert_eq!(cat.text, "My cat Biscuit is twelve years old.");
    assert_eq!((cat.kind, cat.trust, cat.scope.clone()), (MemoryKind::Note, Trust::User, Scope::Global));
    assert_eq!(cat.occurred_at_ms, T_OLD + 100, "dated by the first checkpoint that carried it");
    assert_eq!(cat.provenance, LEGACY_CONVERSATION_PROVENANCE);
    assert!(cat.id.starts_with("m-old-a-"), "filed under the event's session: {}", cat.id);
    let coffee = find("dark roast coffee").remove(0);
    assert_eq!((coffee.kind, coffee.occurred_at_ms), (MemoryKind::Note, T_OLD + 200));
    let time: Vec<RecallHit> = find("what time").into_iter().filter(|h| h.text == "What time is it?").collect();
    assert_eq!(time.len(), 1, "deduplicated by text across sessions: {time:?}");
    assert_eq!((time[0].kind, time[0].trust), (MemoryKind::Episode, Trust::User));
    assert!(time[0].id.starts_with("m-old-a-"), "first-seen session wins: {}", time[0].id);
    let readme = find("summarise README").remove(0);
    assert_eq!((readme.kind, readme.trust), (MemoryKind::Episode, Trust::User));
    assert!(readme.id.starts_with("m-old-b-"), "{}", readme.id);
    let noon = find("noon Porto").remove(0);
    assert_eq!((noon.kind, noon.trust), (MemoryKind::Episode, Trust::Inferred), "assistant prose");
    for machine in ["budget.rs Enforces", "spawned tools budget", "output contract findings", "release-notes skill", "Cargo.toml"] {
        let hits = find(machine);
        assert!(hits.iter().all(|h| h.provenance != LEGACY_CONVERSATION_PROVENANCE), "{machine}: {hits:?}");
    }
    // Conversation turns are not session summaries.
    assert!(mind.episodes(0..i64::MAX).unwrap().is_empty());

    let again = mind.import_legacy(&old, &mut |_| {}).unwrap();
    assert_eq!((again.imported, again.skipped_duplicates, again.conversation_turns_imported), (0, 9, 0), "{again:?}");
    assert!(mind.legacy_status(Some(&old)).unwrap().unwrap().already_imported);
    assert_eq!(mind.stats().unwrap().beliefs, 3 + 5);
    mind.shutdown().unwrap();
    assert_eq!(digest_dir(&old), before, "the old profile must never be written");
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(old.parent().unwrap());
}

#[test]
fn a_forgotten_imported_turn_stays_forgotten_on_reimport() {
    let old = temp_home("legacy-talk-forget").join("default-profile");
    synthetic_old_profile_with_conversation(&old);
    let home = temp_home("legacy-talk-forget-new");
    let mind = Mind::open_with(&home, without_models(&home)).unwrap();
    mind.import_legacy(&old, &mut |_| {}).unwrap();
    let cat = mind.recall(RecallQuery { text: "Biscuit".into(), scope: None, limit: 1 }).unwrap().remove(0);
    mind.forget(&cat.id, user()).unwrap();
    let again = mind.import_legacy(&old, &mut |_| {}).unwrap();
    assert_eq!(again.imported, 0, "{again:?}");
    assert!(mind.recall(RecallQuery { text: "Biscuit".into(), scope: None, limit: 1 }).unwrap().is_empty());
    drop(mind);
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(old.parent().unwrap());
}

#[test]
fn a_retraction_hides_the_rewound_turns_from_every_path_and_survives_a_reopen() {
    let home = temp_home("retract");
    let said = |mind: &Mind, text: &str, at: i64| {
        mind.ingest_turn(TurnIn { session: "s1".into(), workspace: None, role: TurnRole::User, text: text.into(), at_ms: at })
            .unwrap();
    };
    let recalled = |mind: &Mind, q: &str| -> Vec<String> {
        mind.recall(RecallQuery { text: q.into(), scope: None, limit: 10 }).unwrap().into_iter().map(|h| h.text).collect()
    };
    let began = {
        let mind = Mind::open_with(&home, without_models(&home)).unwrap();
        said(&mind, "My sister lives in Porto.", 1);
        let began = mind.journal_seq().unwrap();
        said(&mind, "My favourite colour is teal.", 2);
        said(&mind, "The staging box is called heron.", 3);
        assert_eq!(recalled(&mind, "teal colour").len(), 1);

        let r = Retraction { session: "s1".into(), turns: vec![1], after_seq: Some(began), from_ms: None };
        assert_eq!(mind.retract(r).unwrap(), 2);
        assert!(recalled(&mind, "teal colour").is_empty(), "retracted words are not recalled");
        assert!(recalled(&mind, "heron staging").is_empty());
        assert_eq!(recalled(&mind, "sister Porto").len(), 1, "what came before the rewound turn stays");
        assert_eq!(mind.stats().unwrap().beliefs, 1);
        began
    };
    // A reopen boots from the post-retract snapshot; deleting the snapshot forces a full replay.
    for replay in [false, true] {
        if replay {
            let _ = fs::remove_file(profile_dir(&home).join(store::SNAPSHOT_FILE));
        }
        let mind = Mind::open_with(&home, without_models(&home)).unwrap();
        assert!(recalled(&mind, "teal colour").is_empty(), "replay={replay}");
        assert_eq!(recalled(&mind, "sister Porto").len(), 1, "replay={replay}");
        assert!(mind.journal_seq().unwrap() > began);
    }
    let _ = fs::remove_dir_all(&home);
}
