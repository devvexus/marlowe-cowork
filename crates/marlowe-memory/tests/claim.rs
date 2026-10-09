//! ADR-038's write path, end to end through a real journal.
//!
//! The property under test is one line — `min(AgentInferred, run_floor)` — and it is exactly the
//! shape that passes on a broken build if asserted carelessly. A test that only checks *"a claim
//! written in a tainted run comes back UntrustedContent"* passes on a build that writes
//! `UntrustedContent` unconditionally, which is the failure in the opposite direction and would
//! make `remember` useless while looking safe.
//!
//! **Every trust assertion here therefore carries its opposite**, in the same test, so neither
//! direction can be green alone.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::{Clock, PayloadKind, TrustClass};
use marlowe_journal::{EventKind, Journal, OperatorCapability, Profile};
use marlowe_memory::{
    remember_claim, BeliefStore, ClaimRejected, ClaimWrite, MATURATION_WINDOW_MS,
};

const T0: i64 = 1_780_000_000_000;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-claim-it-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

struct Fixture {
    dir: PathBuf,
    journal: Journal,
    beliefs: BeliefStore,
}

fn fixture(name: &str) -> Fixture {
    let dir = tmp(name);
    let profile = Profile::init(&dir).unwrap();
    let journal = Journal::open(&profile).unwrap();
    Fixture { dir, journal, beliefs: BeliefStore::default() }
}

fn claim<'a>(session: &'a str, text: &'a str, floor: TrustClass) -> ClaimWrite<'a> {
    ClaimWrite {
        session_id: session,
        run_id: "run-1",
        text,
        payload_kind: PayloadKind::Fact,
        derived_from: &[],
        run_floor: floor,
    }
}

/// **The ADR-038 property, with both directions in one test.**
///
/// Split into two tests, each asserting one direction, both would pass on a build that ignored the
/// floor and returned a constant — one of them, at least, and a suite is read by whether anything
/// is red. Asserting the *difference* is what makes the floor observable.
#[test]
fn the_run_floor_decides_a_claims_trust_class_and_a_clean_run_writes_higher() {
    let mut f = fixture("floor");

    let tainted = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-dirty", "the release train leaves Thursday", TrustClass::UntrustedContent),
    )
    .unwrap()
    .unwrap();

    let clean = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-clean", "the release train leaves Thursday", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();

    assert_eq!(
        tainted.effective_trust,
        TrustClass::UntrustedContent,
        "a run that had read untrusted content must not write above its own floor"
    );
    assert_eq!(
        clean.effective_trust,
        TrustClass::AgentInferred,
        "a clean run writes at AgentInferred -- the model concluded it. Never AgentObserved, \
         which is the tier reserved for what the HARNESS computed"
    );
    assert!(
        clean.effective_trust > tainted.effective_trust,
        "identical text, identical clock, different floor: if these are equal the floor is not \
         reaching the write path at all"
    );
}

/// A claim never reaches `AgentObserved`, whatever the floor.
///
/// `UserAsserted` is the highest floor a run can have, and the `min` caps the result at
/// `AgentInferred`. Without the cap a clean run's claim would land at the floor itself — the model
/// writing prose at the tier §3.3 reserves for exit codes and hashes.
#[test]
fn no_floor_however_high_lets_a_model_claim_reach_the_harness_tier() {
    let mut f = fixture("cap");
    for floor in [TrustClass::UserAsserted, TrustClass::AgentObserved, TrustClass::AgentInferred] {
        let r = remember_claim(
            &mut f.journal,
            &mut f.beliefs,
            Clock::new(T0),
            &claim("s-cap", "a claim", floor),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            r.effective_trust,
            TrustClass::AgentInferred,
            "floor {floor:?} produced {:?}; the cap is AgentInferred",
            r.effective_trust
        );
    }
}

/// §3.3's `min` over lineage, unchanged, on top of ADR-038's cap.
#[test]
fn a_parents_worse_trust_drags_the_derived_claim_down_with_it() {
    let mut f = fixture("lineage");

    let parent = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-lin", "a page said this", TrustClass::UntrustedContent),
    )
    .unwrap()
    .unwrap();
    assert_eq!(parent.effective_trust, TrustClass::UntrustedContent);

    let parents = vec![parent.id.clone()];
    // A CLEAN run -- floor UserAsserted -- deriving from an untrusted parent. Own trust would be
    // AgentInferred; the lineage is what must pull it down. If §3.3's min were skipped this would
    // come back AgentInferred and the laundering path would be open through derivation.
    let derived = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &ClaimWrite {
            session_id: "s-lin",
            run_id: "run-2",
            text: "therefore the deadline is Thursday",
            payload_kind: PayloadKind::Fact,
            derived_from: &parents,
            run_floor: TrustClass::UserAsserted,
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(
        derived.effective_trust,
        TrustClass::UntrustedContent,
        "a belief derived from an untrusted parent is untrusted, however clean the run that \
         derived it. This is HP6: content signals do not survive derivation"
    );
}

/// A refusal is **journalled**, not inferred from absence.
///
/// §4.6's rule for ingest, applied here: *"a suite that plants a malformed or unauthorized write
/// must be able to see it refused rather than infer refusal from absence."* A `remember` that
/// silently did nothing is indistinguishable from one that was never attempted.
#[test]
fn an_unknown_parent_is_refused_and_the_refusal_is_in_the_log() {
    let mut f = fixture("unknown-parent");
    let parents = vec!["m-does-not-exist".to_string()];

    let outcome = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &ClaimWrite {
            session_id: "s-u",
            run_id: "run-3",
            text: "a claim with a fabricated lineage",
            payload_kind: PayloadKind::Fact,
            derived_from: &parents,
            run_floor: TrustClass::UserAsserted,
        },
    )
    .unwrap();

    assert_eq!(outcome, Err(ClaimRejected::UnknownParent("m-does-not-exist".to_string())));
    assert!(f.beliefs.recall_candidates().is_empty(), "nothing may have been written");

    let kinds: Vec<EventKind> = f
        .journal
        .replay(&OperatorCapability::for_operator_or_audit(), None)
        .unwrap()
        .into_iter()
        .map(|(_, kind, _)| kind)
        .collect();
    assert!(
        kinds.contains(&EventKind::MemoryWriteRejected),
        "the refusal must be visible in the log, not inferred from the absence of a write: {kinds:?}"
    );
    assert!(
        !kinds.contains(&EventKind::MemoryWritten),
        "a refused claim must not also appear as written"
    );

    let _ = fs::remove_dir_all(&f.dir);
}

// ── correct / forget — CONTRACTS §3.5's other two methods ─────────────────────────────

/// A correction supersedes, and the superseded belief leaves auto-injection but stays recallable.
#[test]
fn a_correction_supersedes_the_old_belief_without_deleting_it() {
    let mut f = fixture("correct");
    let old = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-c", "the deploy runs on Fridays", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();

    let new = marlowe_memory::correct_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-c", "the deploy runs on Tuesdays", TrustClass::UserAsserted),
        &old.id,
    )
    .unwrap()
    .unwrap();

    let matured = T0 + MATURATION_WINDOW_MS + 1;
    let injectable: Vec<&str> =
        f.beliefs.injection_candidates(matured).iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        injectable,
        vec![new.id.as_str()],
        "§4.3 exclusion 2 removes the superseded belief from auto-injection"
    );
    // **Still recallable.** Correction is not deletion — "I used to think X" is exactly what §3.6's
    // explicit path exists to answer, and a correction that erased the old value would make the
    // system unable to explain itself.
    assert_eq!(f.beliefs.recall_candidates().len(), 2);
    assert_eq!(f.beliefs.get(&old.id).unwrap().superseded_by.as_deref(), Some(new.id.as_str()));

    let _ = fs::remove_dir_all(&f.dir);
}

/// **The new rule, with both directions.** A tainted run may not evict a better-sourced belief.
#[test]
fn a_correction_may_not_lower_a_beliefs_authority_but_an_equal_one_may() {
    let mut f = fixture("authority");
    let trusted = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-a", "the release train leaves Thursday", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();
    assert_eq!(trusted.effective_trust, TrustClass::AgentInferred);

    // A run that has read a page writes at UntrustedContent — below the target.
    let refused = marlowe_memory::correct_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-a", "actually it leaves Monday", TrustClass::UntrustedContent),
        &trusted.id,
    )
    .unwrap();
    assert!(
        matches!(refused, Err(ClaimRejected::WouldLowerAuthority { .. })),
        "a tainted run must not evict a better-sourced belief: {refused:?}"
    );
    assert!(f.beliefs.get(&trusted.id).unwrap().superseded_by.is_none(), "and nothing moved");

    // The control: an equally-sourced correction is allowed, so the refusal above is about
    // authority and not about corrections never working.
    let allowed = marlowe_memory::correct_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-a", "it leaves Monday", TrustClass::UserAsserted),
        &trusted.id,
    )
    .unwrap();
    assert!(allowed.is_ok(), "an equal-authority correction must work: {allowed:?}");

    let _ = fs::remove_dir_all(&f.dir);
}

/// Forgetting clears the text, leaves the record, and survives a rebuild identically.
///
/// **The rebuild half is the one with teeth.** `tombstone` and `derive` are two paths folding one
/// event; if they disagreed, the store would look right until the next restart and then change
/// under the user — invisible in every in-process test.
#[test]
fn forgetting_clears_the_text_and_a_rebuild_reproduces_exactly_that() {
    let mut f = fixture("forget");
    let a = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-f", "a secret worth forgetting", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();

    marlowe_memory::forget_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        "s-f",
        "run-f",
        &a.id,
    )
    .unwrap()
    .unwrap();

    let live = f.beliefs.get(&a.id).unwrap();
    assert!(live.text.is_empty(), "the text is gone");
    assert_eq!(live.fidelity, marlowe_contract::Fidelity::Tombstone);
    assert!(
        f.beliefs.injection_candidates(T0 + MATURATION_WINDOW_MS + 1).is_empty(),
        "§4.3 exclusion 1: a tombstone never competes for injection"
    );

    let rebuilt = BeliefStore::derive(&f.journal, marlowe_memory::DERIVATION_VERSION).unwrap();
    let after = rebuilt.get(&a.id).expect("the RECORD survives; only the text is cleared");
    assert!(after.text.is_empty(), "the fold must reach the same state as the live path");
    assert_eq!(after.fidelity, marlowe_contract::Fidelity::Tombstone);

    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn correcting_or_forgetting_something_that_does_not_exist_is_refused() {
    let mut f = fixture("unknown-target");
    let missing = "m-nope";
    assert_eq!(
        marlowe_memory::forget_claim(
            &mut f.journal,
            &mut f.beliefs,
            Clock::new(T0),
            "s",
            "r",
            missing
        )
        .unwrap(),
        Err(ClaimRejected::UnknownParent(missing.to_string()))
    );
    assert_eq!(
        marlowe_memory::correct_claim(
            &mut f.journal,
            &mut f.beliefs,
            Clock::new(T0),
            &claim("s", "a replacement", TrustClass::UserAsserted),
            missing,
        )
        .unwrap(),
        Err(ClaimRejected::UnknownParent(missing.to_string()))
    );
    assert!(
        f.beliefs.recall_candidates().is_empty(),
        "a refused correction must not leave its replacement behind"
    );
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn an_empty_claim_is_refused_rather_than_written() {
    let mut f = fixture("empty");
    let outcome =
        remember_claim(&mut f.journal, &mut f.beliefs, Clock::new(T0), &claim("s-e", "   ", TrustClass::UserAsserted))
            .unwrap();
    assert_eq!(outcome, Err(ClaimRejected::EmptyClaim));
    assert!(
        f.beliefs.recall_candidates().is_empty(),
        "an empty belief is retrievable, scores against every query, and asserts nothing"
    );
}

/// §4.3 exclusion (3). A claim is not injectable the instant it is written.
///
/// This is the cheapest available defence against single-exposure poisoning, and §4.3 warns that an
/// implementation dropping it *"has silently removed that defence while still passing every latency
/// and precision test."*
#[test]
fn a_fresh_claim_is_withheld_from_injection_until_it_matures() {
    let mut f = fixture("maturation");
    let r = remember_claim(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &claim("s-m", "a fresh claim", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();

    assert_eq!(r.silent_until, T0 + MATURATION_WINDOW_MS);
    assert!(
        f.beliefs.injection_candidates(T0).is_empty(),
        "a claim written now must not be an injection candidate now"
    );
    assert_eq!(
        f.beliefs.injection_candidates(T0 + MATURATION_WINDOW_MS + 1).len(),
        1,
        "and it must become one afterwards, or this test would pass on a store that never \
         injects anything"
    );
    // Explicit recall sees it immediately -- CONTRACTS §3.6, and the reason the research-memory
    // scenario does not have to wait six hours.
    assert_eq!(f.beliefs.recall_candidates().len(), 1);
}

/// Ids carry no clock, so two identical replays produce identical ids.
///
/// `entry::memory_ids_do_not_contain_a_timestamp` states the rule; clock probe test A is what
/// enforces it across a ten-year shift. A claim id containing a `RunId` would break the same
/// property, which is why the run reaches the entry and never the id.
#[test]
fn the_same_write_against_the_same_store_gets_the_same_id_at_any_clock() {
    let mut a = fixture("id-a");
    let mut b = fixture("id-b");

    let one = remember_claim(
        &mut a.journal,
        &mut a.beliefs,
        Clock::new(T0),
        &claim("s-id", "same text", TrustClass::UserAsserted),
    )
    .unwrap()
    .unwrap();

    let two = remember_claim(
        &mut b.journal,
        &mut b.beliefs,
        Clock::new(T0 + 10 * 365 * 24 * 60 * 60 * 1000),
        &ClaimWrite {
            session_id: "s-id",
            // A DIFFERENT run: the id must not depend on it, or a replay whose RunId differs
            // would produce a different store.
            run_id: "run-99",
            text: "same text",
            payload_kind: PayloadKind::Fact,
            derived_from: &[],
            run_floor: TrustClass::UserAsserted,
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(one.id, two.id, "ten years and a different run id may not move an id");

    let _ = fs::remove_dir_all(&a.dir);
    let _ = fs::remove_dir_all(&b.dir);
}

/// Two claims in one session do not collide.
#[test]
fn successive_claims_in_a_session_get_distinct_ids() {
    let mut f = fixture("distinct");
    let mut ids = Vec::new();
    for text in ["first", "second", "third"] {
        ids.push(
            remember_claim(
                &mut f.journal,
                &mut f.beliefs,
                Clock::new(T0),
                &claim("s-d", text, TrustClass::UserAsserted),
            )
            .unwrap()
            .unwrap()
            .id,
        );
    }
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 3, "{ids:?}");
    assert_eq!(f.beliefs.recall_candidates().len(), 3);

    let _ = fs::remove_dir_all(&f.dir);
}
