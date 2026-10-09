//! The product write path through a real journal: trust from the author, maturation by trust
//! class, assistant prose kept out of injection, corrections that cannot lower authority.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::{Clock, TrustClass};
use marlowe_journal::{EventKind, Journal, OperatorCapability, Profile, DERIVATION_VERSION};
use marlowe_memory::{
    correct_belief, write_belief, Author, BeliefKind, BeliefScope, BeliefStore, BeliefWrite,
    ClaimRejected, MaturationPolicy, Tags, Utterance, MATURATION_WINDOW_MS,
};

const T0: i64 = 1_780_000_000_000;

/// The temp dir is cleared at the start of each run (Windows will not delete an open journal).
struct Fixture {
    _profile: Profile,
    journal: Journal,
    beliefs: BeliefStore,
}

fn fixture(name: &str) -> Fixture {
    let dir: PathBuf = std::env::temp_dir().join(format!("marlowe-write-it-{name}"));
    let _ = fs::remove_dir_all(&dir);
    let profile = Profile::init(&dir).unwrap();
    let journal = Journal::open(&profile).unwrap();
    Fixture { _profile: profile, journal, beliefs: BeliefStore::default() }
}

fn w<'a>(text: &'a str, author: Author, tags: Tags) -> BeliefWrite<'a> {
    BeliefWrite {
        session_id: "tui",
        turn_id: "turn-1",
        run_id: None,
        text,
        author,
        derived_from: &[],
        occurred_at_ms: T0,
        tags,
        maturation: MaturationPolicy::ByTrust,
    }
}

fn fact() -> Tags {
    Tags { kind: Some(BeliefKind::Fact), ..Tags::default() }
}

#[test]
fn a_user_belief_is_live_at_once_and_an_agent_one_waits_out_the_window() {
    let mut f = fixture("maturation");
    let clock = Clock::new(T0);
    let user = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("I use Windows ARM64", Author::User, fact()))
        .unwrap()
        .unwrap();
    let agent = write_belief(
        &mut f.journal,
        &mut f.beliefs,
        clock,
        &w("Matthew probably uses Windows ARM64", Author::Agent { run_floor: TrustClass::UserAsserted }, fact()),
    )
    .unwrap()
    .unwrap();

    assert_eq!(user.effective_trust, TrustClass::UserAsserted);
    assert_eq!(user.silent_until, None);
    assert_eq!(agent.effective_trust, TrustClass::AgentInferred);
    assert_eq!(agent.silent_until, Some(T0 + MATURATION_WINDOW_MS));

    let live: Vec<&str> = f.beliefs.injection_candidates(T0).iter().map(|e| e.id.as_str()).collect();
    assert_eq!(live, vec![user.id.as_str()], "only the user's belief is injectable at write time");
    assert_eq!(f.beliefs.injection_candidates(T0 + MATURATION_WINDOW_MS).len(), 2);
}

#[test]
fn untrusted_lineage_keeps_the_window_even_for_a_user_authored_belief() {
    let mut f = fixture("lineage");
    let clock = Clock::new(T0);
    let page = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("the page says X", Author::External, fact()))
        .unwrap()
        .unwrap();
    assert_eq!(page.effective_trust, TrustClass::UntrustedContent);
    assert_eq!(page.silent_until, Some(T0 + MATURATION_WINDOW_MS));

    let parents = [page.id.clone()];
    let derived = BeliefWrite { derived_from: &parents, ..w("so X is true", Author::User, fact()) };
    let r = write_belief(&mut f.journal, &mut f.beliefs, clock, &derived).unwrap().unwrap();
    assert_eq!(r.trust_class, TrustClass::UserAsserted, "own class is the author's");
    assert_eq!(r.effective_trust, TrustClass::UntrustedContent, "worst case over lineage");
    assert_eq!(r.silent_until, Some(T0 + MATURATION_WINDOW_MS), "and maturation follows effective trust");

    // A tainted run writes untrusted no matter what it claims.
    let tainted = write_belief(
        &mut f.journal,
        &mut f.beliefs,
        clock,
        &w("noted", Author::Agent { run_floor: TrustClass::UntrustedContent }, fact()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(tainted.effective_trust, TrustClass::UntrustedContent);
}

#[test]
fn the_uniform_policy_withholds_even_the_user() {
    let mut f = fixture("uniform");
    let write = BeliefWrite { maturation: MaturationPolicy::Uniform, ..w("hello", Author::User, fact()) };
    let r = write_belief(&mut f.journal, &mut f.beliefs, Clock::new(T0), &write).unwrap().unwrap();
    assert_eq!(r.silent_until, Some(T0 + MATURATION_WINDOW_MS));
    assert!(f.beliefs.injection_candidates(T0).is_empty());
}

#[test]
fn assistant_prose_is_recallable_but_never_injected_and_a_summary_is_injected() {
    let mut f = fixture("prose");
    let clock = Clock::new(T0);
    let user_turn = Tags { kind: Some(BeliefKind::Episode), utterance: Some(Utterance::User), ..Tags::default() };
    let reply = Tags { kind: Some(BeliefKind::Episode), utterance: Some(Utterance::Assistant), ..Tags::default() };
    let summary = Tags { kind: Some(BeliefKind::Episode), ..Tags::default() };
    let floor = TrustClass::UserAsserted;

    let u = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("can you fix the build?", Author::User, user_turn)).unwrap().unwrap();
    let a = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("Done, the build is fixed.", Author::Agent { run_floor: floor }, reply)).unwrap().unwrap();
    let s = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("Session: fixed the ARM64 build.", Author::Agent { run_floor: floor }, summary)).unwrap().unwrap();

    let later = T0 + MATURATION_WINDOW_MS;
    let live: Vec<&str> = f.beliefs.injection_candidates(later).iter().map(|e| e.id.as_str()).collect();
    assert!(live.contains(&u.id.as_str()));
    assert!(live.contains(&s.id.as_str()), "an episode summary block is injectable");
    assert!(!live.contains(&a.id.as_str()), "assistant prose never is");
    assert!(f.beliefs.recall_candidates().iter().any(|e| e.id == a.id), "but it is recallable");
    assert_eq!(f.beliefs.get(&a.id).unwrap().kind(), BeliefKind::Episode);
    assert_eq!(f.beliefs.get(&a.id).unwrap().provenance().describe(), "assistant reply (session tui)");
}

#[test]
fn an_import_keeps_its_classes_and_cannot_raise_them() {
    let mut f = fixture("import");
    let tags = Tags { kind: Some(BeliefKind::Fact), source: Some("imported from Marlowe_Harness".into()), ..Tags::default() };
    let kept = write_belief(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &BeliefWrite {
            turn_id: "import-1",
            occurred_at_ms: T0 - 86_400_000,
            ..w("old fact", Author::Imported { trust_class: TrustClass::AgentObserved, effective_trust: TrustClass::AgentObserved }, tags.clone())
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!((kept.trust_class, kept.effective_trust), (TrustClass::AgentObserved, TrustClass::AgentObserved));
    let entry = f.beliefs.get(&kept.id).unwrap();
    assert_eq!(entry.occurred_at_ms, T0 - 86_400_000, "the original time is carried");
    assert_eq!(entry.provenance().describe(), "imported from Marlowe_Harness");

    let inconsistent = write_belief(
        &mut f.journal,
        &mut f.beliefs,
        Clock::new(T0),
        &BeliefWrite {
            turn_id: "import-2",
            ..w("laundered?", Author::Imported { trust_class: TrustClass::UntrustedContent, effective_trust: TrustClass::UserAsserted }, tags)
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(inconsistent.effective_trust, TrustClass::UntrustedContent, "effective never exceeds own");
}

#[test]
fn the_user_may_correct_the_agent_but_a_tainted_run_may_not_correct_the_user() {
    let mut f = fixture("correct");
    let clock = Clock::new(T0);
    let mine = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("standup is at 9", Author::User, fact())).unwrap().unwrap();
    let theirs = write_belief(
        &mut f.journal,
        &mut f.beliefs,
        clock,
        &w("standup is at 10", Author::Agent { run_floor: TrustClass::UserAsserted }, fact()),
    )
    .unwrap()
    .unwrap();

    let refused = correct_belief(
        &mut f.journal,
        &mut f.beliefs,
        clock,
        &w("standup is at 11", Author::Agent { run_floor: TrustClass::UntrustedContent }, fact()),
        &mine.id,
    )
    .unwrap();
    assert!(matches!(refused, Err(ClaimRejected::WouldLowerAuthority { .. })));
    assert!(f.beliefs.get(&mine.id).unwrap().superseded_by.is_none());

    let fixed = correct_belief(&mut f.journal, &mut f.beliefs, clock, &w("standup is at 9:30", Author::User, fact()), &theirs.id)
        .unwrap()
        .unwrap();
    assert_eq!(f.beliefs.get(&theirs.id).unwrap().superseded_by.as_deref(), Some(fixed.id.as_str()));
    let live: Vec<&str> = f.beliefs.injection_candidates(T0).iter().map(|e| e.id.as_str()).collect();
    assert!(!live.contains(&theirs.id.as_str()));
    assert!(live.contains(&fixed.id.as_str()));

    // The rebuild agrees with the live store.
    let rebuilt = BeliefStore::derive(&f.journal, DERIVATION_VERSION).unwrap();
    assert_eq!(rebuilt.entries().cloned().collect::<Vec<_>>(), f.beliefs.entries().cloned().collect::<Vec<_>>());
}

#[test]
fn ids_are_positional_distinct_and_a_refusal_is_journalled() {
    let mut f = fixture("ids");
    let clock = Clock::new(T0);
    let scoped = Tags { kind: Some(BeliefKind::Note), scope: BeliefScope::Project("/w".into()), ..Tags::default() };
    let a = write_belief(&mut f.journal, &mut f.beliefs, clock, &w("one", Author::User, scoped.clone())).unwrap().unwrap();
    let b = write_belief(&mut f.journal, &mut f.beliefs, Clock::new(T0 + 999_999), &w("two", Author::User, scoped)).unwrap().unwrap();
    assert_eq!(a.id, "m-tui-turn-1-0");
    assert_eq!(b.id, "m-tui-turn-1-1", "no clock in the id");
    assert!(f.beliefs.get(&a.id).unwrap().scope().admits(Some("/w")));
    assert!(!f.beliefs.get(&a.id).unwrap().scope().admits(Some("/elsewhere")));

    let parents = ["m-nope".to_string()];
    let orphan = BeliefWrite { derived_from: &parents, ..w("child", Author::User, fact()) };
    assert_eq!(
        write_belief(&mut f.journal, &mut f.beliefs, clock, &orphan).unwrap(),
        Err(ClaimRejected::UnknownParent("m-nope".into()))
    );
    let cap = OperatorCapability::for_operator_or_audit();
    assert_eq!(f.journal.replay(&cap, Some(EventKind::MemoryWriteRejected)).unwrap().len(), 1);
}
