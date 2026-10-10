//! `/rewind` un-says a session's rewound turns: the `Retracted` event keeps their beliefs in the
//! journal and out of retrieval, and a replay or a snapshot plus tail derives the same view.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::Clock;
use marlowe_journal::{Journal, Profile, DERIVATION_VERSION};
use marlowe_memory::store::{append_retraction, RetractedPayload, RETRACTED_BY_PREFIX};
use marlowe_memory::{
    write_belief, Author, BeliefKind, BeliefSnapshot, BeliefStore, BeliefWrite, MaturationPolicy, MemoryEntry, Tags,
};

const T0: i64 = 1_780_000_000_000;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-retract-it-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn say(journal: &mut Journal, beliefs: &mut BeliefStore, session: &str, text: &str, at: i64) -> String {
    let w = BeliefWrite {
        session_id: session,
        turn_id: "user",
        run_id: None,
        text,
        author: Author::User,
        derived_from: &[],
        occurred_at_ms: at,
        tags: Tags { kind: Some(BeliefKind::Note), ..Tags::default() },
        maturation: MaturationPolicy::ByTrust,
    };
    write_belief(journal, beliefs, Clock::new(at), &w).unwrap().unwrap().id
}

fn entries(store: &BeliefStore) -> Vec<MemoryEntry> {
    store.entries().cloned().collect()
}

fn candidates(store: &BeliefStore) -> Vec<String> {
    let mut ids: Vec<String> = store.injection_candidates(T0 + 10_000_000).iter().map(|e| e.id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn rewound_turns_leave_retrieval_and_replay_agrees() {
    let dir = tmp("seq");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut live = BeliefStore::default();

    let kept = say(&mut journal, &mut live, "s", "my sister lives in Porto", T0);
    let watermark = journal.last_seq();
    let sealed = live.snapshot(watermark).seal(profile.key()).unwrap();

    let began = journal.last_seq();
    let gone_a = say(&mut journal, &mut live, "s", "my favourite colour is teal", T0 + 10);
    let gone_b = say(&mut journal, &mut live, "s", "the staging box is called heron", T0 + 20);
    let other = say(&mut journal, &mut live, "other", "my dog is called Biscuit", T0 + 30);
    assert_eq!(candidates(&live).len(), 4);

    let p = RetractedPayload { session: "s".into(), turns: vec![1, 2], after_seq: Some(began), from_ms: None };
    let n = append_retraction(&mut journal, &mut live, Clock::new(T0 + 40), &p).unwrap();
    assert_eq!(n, 2, "only the session's entries written after the rewound turn began");

    let mut want = vec![kept.clone(), other.clone()];
    want.sort();
    assert_eq!(candidates(&live), want);
    let marked = live.get(&gone_a).unwrap();
    assert!(marked.superseded_by.as_deref().unwrap().starts_with(RETRACTED_BY_PREFIX));
    assert_eq!(marked.text, "my favourite colour is teal", "retracted, not forgotten: the journal keeps the words");
    assert!(live.get(&gone_b).unwrap().superseded_by.is_some());

    // A replay of the journal, and a snapshot taken before the rewind plus its tail, agree.
    let full = BeliefStore::derive(&journal, DERIVATION_VERSION).unwrap();
    let snap = BeliefSnapshot::open(&sealed, profile.key()).unwrap();
    let booted = BeliefStore::from_snapshot(snap, &journal, DERIVATION_VERSION).unwrap();
    assert_eq!(entries(&full), entries(&live));
    assert_eq!(entries(&booted), entries(&live));

    // Retracting again changes nothing.
    assert_eq!(append_retraction(&mut journal, &mut live, Clock::new(T0 + 50), &p).unwrap(), 0);

    drop(journal);
    drop(profile);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_session_saved_before_seq_tracking_retracts_by_time() {
    let dir = tmp("ms");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut live = BeliefStore::default();
    let early = say(&mut journal, &mut live, "s", "I use Helix", T0);
    let late = say(&mut journal, &mut live, "s", "I switched to Kakoune", T0 + 100);

    let p = RetractedPayload { session: "s".into(), turns: vec![3], after_seq: None, from_ms: Some(T0 + 50) };
    assert_eq!(append_retraction(&mut journal, &mut live, Clock::new(T0 + 200), &p).unwrap(), 1);
    assert!(live.get(&early).unwrap().superseded_by.is_none());
    assert!(live.get(&late).unwrap().superseded_by.is_some());
    assert_eq!(entries(&BeliefStore::derive(&journal, DERIVATION_VERSION).unwrap()), entries(&live));

    // A payload naming neither bound retracts nothing rather than everything.
    let none = RetractedPayload { session: "s".into(), turns: vec![], after_seq: None, from_ms: None };
    assert_eq!(append_retraction(&mut journal, &mut live, Clock::new(T0 + 300), &none).unwrap(), 0);

    drop(journal);
    drop(profile);
    let _ = fs::remove_dir_all(&dir);
}
