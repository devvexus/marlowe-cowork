//! **One belief per source, when a whole group is read in one turn at one clock reading.**
//!
//! ADR-041 made the quarantined read a *group*: up to [`marlowe_loop::MAX_SOURCES_PER_READER`]
//! fetched pages are condensed by one child, in one turn, against one reading of the run's clock.
//! Whatever eventually calls [`MemoryHost::ingest_external`] therefore calls it N times with the
//! same `run` and the same `now_ms` — that is not an unlikely edge, it is the shape of the only
//! caller ADR-041 permits.
//!
//! # What actually collided, established before the fix rather than asserted after it
//!
//! `DaemonMemory::ingest_external` built `turn_id: format!("external:{run}:{now_ms}")` and sent a
//! **one-turn** `IngestRequest`. `marlowe_memory::entry::memory_id` is
//! `m-{session_id}-{turn_id}-{index}`, and `index` is the turn's position *within one request*, so
//! it is `0` on every one of those calls. Six calls therefore derived **one** id, and
//! `BeliefStore::insert` is a `BTreeMap` insert — last write wins, silently. Six sources became
//! one belief with no error, no `MemoryWriteRejected` event, and nothing failing.
//!
//! So the collision is in the **`turn_id`**, the loss is in the **derived `memory_id`** and in the
//! **store key** it becomes. The `turn_id` is where it is fixed, because that is the only one of
//! the three this crate owns.
//!
//! # The vacuity guard on this file
//!
//! These tests are worth nothing if `ingest_external` refuses every call for an unrelated reason —
//! six refusals also produce a store of size zero, and `assert_eq!(len, 6)` failing would look
//! identical to the bug. Every call's `Result` is unwrapped, and
//! [`the_control_one_source_is_one_belief`] establishes that a single call writes exactly one
//! belief on this fixture, so a `6` that came from the fix is distinguishable from a `6` that
//! could never have been anything else.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marlowe_contract::{Channel, TrustClass};
use marlowe_daemon::memory::DaemonMemory;
use marlowe_journal::{Journal, Profile};
use marlowe_loop::driver::{ExternalContent, MemoryHost};
use marlowe_loop::run::{RunId, SessionId};
use marlowe_memory::{DERIVATION_VERSION, MATURATION_WINDOW_MS};

/// One clock reading, used for every call in a group. **The point of the file** — a test that
/// advanced the clock between calls would pass on the unfixed code.
const T0: i64 = 1_780_000_000_000;

/// `std::process::id()` is not decoration. Two runs of this binary — a second worktree, or a re-run
/// started while one is live, both of which CLAUDE.md's parallel-sessions section warns about —
/// would otherwise `remove_dir_all` each other's profile mid-test, and
/// `a_group_read_at_one_clock_reading_writes_one_belief_per_source` would report a wrong belief
/// COUNT rather than an obvious I/O error. `the_model_knows_where_it_is.rs` and `reasoning_leak.rs`
/// already do this.
fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "marlowe-daemon-ext-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Write-only, exactly as `memory_durability.rs` opens it: these tests are about the WRITE path,
/// and loading a cross-encoder to exercise it would make a missing model look like a write failure.
fn open(root: &PathBuf) -> DaemonMemory {
    open_with_journal(root).0
}

/// The same open, keeping a handle on the journal the store is a view over. `forget_claim` writes
/// an EVENT and folds it into the store, so a test about forgetting needs both halves — and using
/// a second `Journal` over the same profile would be testing two stores, not one.
fn open_with_journal(root: &PathBuf) -> (DaemonMemory, Arc<Mutex<Journal>>) {
    let profile = if root.join("profile.json").exists() {
        Profile::open(root).unwrap()
    } else {
        Profile::init(root).unwrap()
    };
    let journal = Arc::new(Mutex::new(Journal::open(&profile).unwrap()));
    let memory = DaemonMemory::open(
        Arc::clone(&journal),
        DERIVATION_VERSION,
        None,
        "test-model",
        marlowe_memory::cue::dense::vram::Tier1Runtime::Ollama,
    )
    .unwrap();
    (memory, journal)
}

/// **The control.** Without it, a `6` below could be a `6` that any implementation would produce.
#[test]
fn the_control_one_source_is_one_belief() {
    let root = tmp("control");
    let mut memory = open(&root);
    memory
        .ingest_external(
            RunId::from_name("r"),
            SessionId::from_name("s"),
            &ExternalContent {
                channel: Channel::Web,
                reference: Some("https://example.invalid/a"),
                text: "the changelog says the API moved to /v3",
            },
            T0,
        )
        .expect("a single external ingest must succeed on this fixture");
    assert_eq!(memory.len(), 1, "one source, one belief");
    let _ = fs::remove_dir_all(&root);
}

/// **The regression.** Fails on the unfixed code with `left: 1, right: 6`.
#[test]
fn a_group_read_at_one_clock_reading_writes_one_belief_per_source() {
    let root = tmp("group");
    let mut memory = open(&root);

    let run = RunId::from_name("the-run-that-read-a-group");
    let session = SessionId::from_name("s");
    let n = marlowe_loop::MAX_SOURCES_PER_READER;

    for i in 0..n {
        let reference = format!("https://example.invalid/page-{i}");
        let text = format!("source {i} says the deploy window moved");
        memory
            .ingest_external(
                run,
                session,
                &ExternalContent {
                    channel: Channel::Web,
                    reference: Some(&reference),
                    text: &text,
                },
                // Same reading for every source. ADR-041 reads the group in ONE turn.
                T0,
            )
            .unwrap_or_else(|e| panic!("source {i} must be ingested, not refused: {e}"));
    }

    assert_eq!(
        memory.len(),
        n,
        "{n} distinct sources were ingested in one turn at one clock reading and the store holds \
         a different number. Every source-derived belief is keyed by a `memory_id` derived from \
         the turn id, so a turn id that does not vary per source makes `BeliefStore::insert` \
         overwrite — silently, with no rejection and no event"
    );

    // The journal side, which the in-memory count alone cannot see. `BeliefStore::derive` rebuilds
    // from the log, so if the six appends carried one id the rebuild folds them back to one even
    // when the live store looked right.
    drop(memory);
    let reopened = open(&root);
    assert_eq!(
        reopened.len(),
        n,
        "the store is a materialized view over the journal; a rebuild must recover the same {n}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// **The deliberate collapse, asserted where it is decided.**
///
/// The fix derives the turn id from `(channel, reference, text)`, so re-ingesting a byte-identical
/// source under a byte-identical origin inside one run is **idempotent** rather than duplicative.
/// That direction is chosen, not incidental: an attacker who can get one page fetched can usually
/// get it fetched repeatedly, and N copies of one belief is N times the apparent corroboration in
/// a store whose retrieval ranks by, among other things, how much of it agrees. Idempotency costs
/// a duplicate; the alternative pays in manufactured consensus.
#[test]
fn the_same_source_ingested_twice_in_one_run_is_one_belief() {
    let root = tmp("idempotent");
    let mut memory = open(&root);
    let run = RunId::from_name("r");
    let session = SessionId::from_name("s");
    let content = ExternalContent {
        channel: Channel::Web,
        reference: Some("https://example.invalid/a"),
        text: "the same summary, twice",
    };

    memory.ingest_external(run, session, &content, T0).unwrap();
    // A LATER clock reading, so this is not merely the same-millisecond case: identity is the
    // content and the origin, and time is not part of it.
    memory
        .ingest_external(run, session, &content, T0 + 60_000)
        .unwrap();

    assert_eq!(
        memory.len(),
        1,
        "identical origin and identical text is one belief"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Two sources differing **only** in `reference` are two beliefs.
///
/// Provenance is part of a belief's identity — CLAUDE.md's saturated-floor paragraph is exactly
/// this point: where every value is `UntrustedContent`, *who asserted it* is the only remaining
/// discriminator, and folding two origins into one entry throws that away.
#[test]
fn identical_text_from_two_references_is_two_beliefs() {
    let root = tmp("refs");
    let mut memory = open(&root);
    let run = RunId::from_name("r");
    let session = SessionId::from_name("s");
    let text = "the maintainer is unreachable this week";

    for reference in ["https://a.invalid/x", "https://b.invalid/y"] {
        memory
            .ingest_external(
                run,
                session,
                &ExternalContent {
                    channel: Channel::Web,
                    reference: Some(reference),
                    text,
                },
                T0,
            )
            .unwrap();
    }

    assert_eq!(
        memory.len(),
        2,
        "same words, different origin — two beliefs, or the journal cannot answer who said it"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The class is still **derived from the channel**, not weakened by any of the above.
///
/// A fix to identity that quietly changed what §3.3 computed would be the more expensive bug, and
/// this is the cheapest possible guard against it. It reads the value `ingest_external` returns,
/// which is what the real `trust_for_channel` produced — not a double's constant.
#[test]
fn a_web_source_is_still_untrusted_content() {
    let root = tmp("class");
    let mut memory = open(&root);
    let derived = memory
        .ingest_external(
            RunId::from_name("r"),
            SessionId::from_name("s"),
            &ExternalContent {
                channel: Channel::Web,
                reference: Some("https://example.invalid/a"),
                text: "a summary of a fetched page",
            },
            T0,
        )
        .unwrap();
    assert_eq!(derived, TrustClass::UntrustedContent);

    // The control: a different channel must derive a different class, or the assertion above is
    // satisfied by any implementation that returns one constant.
    let terminal = memory
        .ingest_external(
            RunId::from_name("r"),
            SessionId::from_name("s"),
            &ExternalContent {
                channel: Channel::Terminal,
                reference: None,
                text: "something the user typed",
            },
            T0,
        )
        .unwrap();
    assert_ne!(
        derived, terminal,
        "the channel is the whole trust decision; if these agree the table is not being read"
    );

    let _ = fs::remove_dir_all(&root);
}

// ══════════════════════════════════════════════════════════════════════════════════════════
// forgetting, which content-derived identity is what makes reachable
// ══════════════════════════════════════════════════════════════════════════════════════════

/// **A forgotten belief must stay forgotten when the same page is fetched again.**
///
/// This is the defect content-derived identity created, and it is the reason the guard in
/// `ingest_external` exists rather than a comment saying idempotency is nice. `BeliefStore::insert`
/// is a `BTreeMap` insert and `BeliefStore::derive` replays with the same insert, so a second
/// `MemoryWritten` for one id **overwrites the entry wholesale** — it restores the text
/// `Tombstoned` cleared, resets `fidelity` to `Record`, and clears `superseded_by`. The concrete
/// sequence: the user says *forget that*, the page is fetched again in the same run, and the
/// forgotten belief is back at full fidelity and readmitted as an injection candidate by §4.3
/// exclusion (1).
///
/// **The journal half is asserted separately**, because the live store and the rebuild are two
/// paths and this is exactly the shape where they disagree silently: a live store that looked right
/// while the log folded back to `Record` would be invisible until a restart.
#[test]
fn a_tombstoned_belief_stays_dead_when_the_same_source_is_ingested_again() {
    let root = tmp("tombstone");
    let (mut memory, journal) = open_with_journal(&root);
    let run = RunId::from_name("r");
    let session = SessionId::from_name("s");
    let content = ExternalContent {
        channel: Channel::Web,
        reference: Some("https://example.invalid/forget-me"),
        text: "the thing the user asked to forget",
    };

    memory.ingest_external(run, session, &content, T0).unwrap();
    let beliefs = memory.beliefs();
    let id = {
        let store = beliefs.lock().unwrap();
        let all = store.recall_candidates();
        assert_eq!(all.len(), 1, "one ingest, one belief, before anything else");
        all[0].id.clone()
    };

    // Forget it, through the real event path — not by mutating the store.
    {
        let mut j = journal.lock().unwrap();
        let mut b = beliefs.lock().unwrap();
        marlowe_memory::forget_claim(
            &mut j,
            &mut b,
            marlowe_contract::Clock::new(T0 + 1_000),
            &session.to_string(),
            &run.to_string(),
            &id,
        )
        .expect("the tombstone must be journalled")
        .expect("the target exists, so the claim must not be rejected");
    }

    // The precondition, asserted rather than assumed: it really is dead before the re-ingest.
    {
        let store = beliefs.lock().unwrap();
        let e = store.get(&id).expect("the entry survives as a tombstone");
        assert_eq!(
            e.fidelity,
            marlowe_contract::Fidelity::Tombstone,
            "the forget must have landed, or the assertion after the re-ingest is vacuous"
        );
        assert!(!e.is_injection_candidate(T0 + MATURATION_WINDOW_MS));
    }

    // The same source, again, in the same run. Without the guard this writes a second
    // `MemoryWritten` under the same derived id and the entry comes back at `Record`.
    memory
        .ingest_external(run, session, &content, T0 + 2_000)
        .expect("a re-ingest of a forgotten source is idempotent, not an error");

    {
        let store = beliefs.lock().unwrap();
        assert_eq!(store.len(), 1, "still one belief");
        let e = store.get(&id).expect("still the same entry");
        assert_eq!(
            e.fidelity,
            marlowe_contract::Fidelity::Tombstone,
            "re-fetching a page must not resurrect a belief the user forgot"
        );
        assert!(
            e.text.is_empty(),
            "the tombstone cleared the text; a re-ingest must not restore it: {:?}",
            e.text
        );
        assert!(
            !e.is_injection_candidate(T0 + MATURATION_WINDOW_MS),
            "a resurrected belief is readmitted by §4.3 exclusion (1), which is the whole harm"
        );
    }

    // The rebuild. The store is a materialized view; if a second event reached the log the fold
    // restores `Record` here even when the live store looked right.
    drop(memory);
    let rebuilt = open(&root);
    let store = rebuilt.beliefs();
    let store = store.lock().unwrap();
    let e = store.get(&id).expect("the rebuild recovers the entry");
    assert_eq!(
        e.fidelity,
        marlowe_contract::Fidelity::Tombstone,
        "the journal rebuild must agree with the live store, or a restart un-forgets it"
    );

    drop(store);
    let _ = fs::remove_dir_all(&root);
}

/// **A supersession edge must survive a re-ingest of the superseded source.**
///
/// `BeliefStore` documents that `superseded_by` and `supersedes` have to agree — §4.3 exclusion
/// (2) reads the first and the audit trail reads the second. An overwrite rewrites only the loser,
/// so the winner would keep `supersedes: [X]` while X's `superseded_by` went back to `None`, and X
/// would be readmitted as an injection candidate with the store asymmetric and nothing complaining.
///
/// **Live-store only, and said so rather than implied.** The supersession is applied with
/// `BeliefStore::supersede` — the path that emits `EventKind::Superseded` is `consolidate.rs`'s and
/// needs a full sweep to reach. So this covers the in-memory half of the invariant; the tombstone
/// test above covers the rebuild half.
#[test]
fn a_supersession_survives_a_re_ingest_of_the_superseded_source() {
    let root = tmp("supersede");
    let mut memory = open(&root);
    let run = RunId::from_name("r");
    let session = SessionId::from_name("s");
    let loser = ExternalContent {
        channel: Channel::Web,
        reference: Some("https://example.invalid/old"),
        text: "the deploy window is Thursday",
    };
    let winner = ExternalContent {
        channel: Channel::Web,
        reference: Some("https://example.invalid/new"),
        text: "the deploy window moved to Monday",
    };

    memory.ingest_external(run, session, &loser, T0).unwrap();
    memory.ingest_external(run, session, &winner, T0).unwrap();

    let beliefs = memory.beliefs();
    let (loser_id, winner_id) = {
        let store = beliefs.lock().unwrap();
        let mut l = None;
        let mut w = None;
        for e in store.recall_candidates() {
            if e.text.contains("Thursday") {
                l = Some(e.id.clone());
            } else {
                w = Some(e.id.clone());
            }
        }
        (
            l.expect("the superseded source was ingested"),
            w.expect("the replacement was ingested"),
        )
    };
    {
        let mut store = beliefs.lock().unwrap();
        assert_eq!(store.len(), 2, "two distinct references, two beliefs");
        store.supersede(&loser_id, &winner_id);
        assert!(
            store.get(&loser_id).unwrap().superseded_by.is_some(),
            "the edge must exist before the re-ingest, or nothing below is a test"
        );
    }

    memory
        .ingest_external(run, session, &loser, T0 + 5_000)
        .unwrap();

    let store = beliefs.lock().unwrap();
    let l = store.get(&loser_id).unwrap();
    assert_eq!(
        l.superseded_by.as_deref(),
        Some(winner_id.as_str()),
        "re-fetching the outdated page must not clear the supersession that replaced it"
    );
    assert!(
        !l.is_injection_candidate(T0 + MATURATION_WINDOW_MS),
        "§4.3 exclusion (2) must still hold"
    );
    assert_eq!(
        store.get(&winner_id).unwrap().supersedes,
        vec![loser_id.clone()],
        "and the two directions of the edge must still agree"
    );

    drop(store);
    let _ = fs::remove_dir_all(&root);
}

// ══════════════════════════════════════════════════════════════════════════════════════════
// the id is a store key, so its format is pinned to a literal
// ══════════════════════════════════════════════════════════════════════════════════════════

/// **The derived id, whole, against a literal.**
///
/// `external_turn_id` encodes the channel through the PINNED serde spelling rather than through
/// `Debug`. A `Debug` derive is not a stable format: renaming `Web` to `WebPage` is a source-only
/// change the contract permits — `#[serde(rename_all = "snake_case")]` holds the wire name — and it
/// would silently change every previously-derived id, so every already-known source would re-ingest
/// as a NEW belief with no error and no event. That is `ingest_external`'s own bug rebuilt one
/// refactor later, and the only thing that catches it is an assertion on the whole string.
///
/// **This test is allowed to fail, and what to do when it does.** If the derivation changes on
/// purpose the existing store forks: every known source becomes a new belief. Re-pin this literal
/// only together with a decision about the beliefs written under the old spelling.
#[test]
fn external_turn_id_is_pinned_to_the_wire_spelling() {
    let root = tmp("pinned");
    let mut memory = open(&root);
    memory
        .ingest_external(
            RunId::from_name("pinned-run"),
            SessionId::from_name("pinned-session"),
            &ExternalContent {
                channel: Channel::Web,
                reference: Some("https://example.invalid/pinned"),
                text: "a pinned summary",
            },
            T0,
        )
        .unwrap();

    let beliefs = memory.beliefs();
    let store = beliefs.lock().unwrap();
    let all = store.recall_candidates();
    assert_eq!(all.len(), 1, "one ingest, one belief");
    let turn_id = all[0].source_turn_id.clone();
    assert_eq!(
        turn_id,
        "external:29232525-1559-5a54-a03e-1b9baa89ec3e:a070ee07-3fbe-5c7e-9f48-072f885bb53f",
        "the derived turn id moved. If that was deliberate, every belief already written under the old derivation is now unreachable by re-ingest and the store forks — re-pin this only with a decision about those"
    );

    drop(store);
    let _ = fs::remove_dir_all(&root);
}
