//! The journal's load-bearing properties, exercised end to end.
//!
//! Every test here corresponds to something a requirement calls structural. If one of these
//! fails, the claim it backs is not true, regardless of what the design documents say.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::Clock;
use marlowe_journal::{
    Actor, AppendRequest, EventKind, Journal, JournalError, OperatorCapability, Profile, TraceId,
};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-journal-it-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn req(kind: EventKind, actor: Actor, payload: serde_json::Value) -> AppendRequest {
    AppendRequest {
        trace_id: TraceId::nil(),
        session_id: Some("s-1".into()),
        run_id: None,
        actor,
        kind,
        payload,
    }
}

#[test]
fn seq_is_monotonic_and_gapless() {
    // Section 1: "Monotonic within a profile. Gaps are impossible; the sequence is the
    // ordering."
    let dir = tmp("seq");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let clock = Clock::new(1_780_000_000_000);

    for i in 0..5 {
        let event = journal
            .append(
                clock,
                req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({ "i": i })),
            )
            .unwrap();
        assert_eq!(event.seq, i + 1);
    }
    assert_eq!(journal.last_seq(), 5);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ts_comes_from_the_supplied_clock_and_nowhere_else() {
    // Section 4.5 is binding on every path reachable from the three interfaces, and ingest
    // reaches append. A timestamp of 1970 is not something a system clock would produce, so
    // this fails loudly if anyone ever reaches for `SystemTime::now`.
    let dir = tmp("clock");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();

    let ancient = Clock::new(86_400_000);
    let event = journal
        .append(ancient, req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({})))
        .unwrap();
    assert_eq!(event.ts, 86_400_000);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_tier_grant_from_the_wrong_actor_is_refused() {
    // Section 1.1's invariant, and section A8.4's "self-granted promotions -- zero,
    // structurally impossible". The model cannot be an Actor at all; this covers the
    // remaining case of a harness component reaching for the wrong one.
    let dir = tmp("tier");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let clock = Clock::new(1_780_000_000_000);

    let refused = journal.append(
        clock,
        req(EventKind::TierGranted, Actor::Harness, serde_json::json!({"tier": 4})),
    );
    assert!(matches!(refused, Err(JournalError::ActorMayNotEmit { .. })));

    let allowed = journal.append(
        clock,
        req(EventKind::TierGranted, Actor::Permission, serde_json::json!({"tier": 4})),
    );
    assert!(allowed.is_ok(), "the permission component may grant");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_edited_payload_breaks_the_chain_on_reopen() {
    // Invariant 7: the log IS the audit trail. If a row can be edited without detection,
    // "every autonomous action is reconstructable" is not true.
    let dir = tmp("tamper");
    // Scoped so the profile's exclusivity lock releases before the reopen below. Two live
    // `Profile` values on one root is the configuration that forks the journal's hash chain.
    {
        let profile = Profile::init(&dir).unwrap();
        let mut journal = Journal::open(&profile).unwrap();
        let clock = Clock::new(1_780_000_000_000);
        for i in 0..3 {
            journal
                .append(
                    clock,
                    req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({"i": i})),
                )
                .unwrap();
        }
    }

    let conn = rusqlite::Connection::open(dir.join("journal.db")).unwrap();
    conn.execute(
        "UPDATE journal SET payload = ?1 WHERE seq = 2",
        rusqlite::params![r#"{"i":999}"#],
    )
    .unwrap();
    drop(conn);

    let profile = Profile::open(&dir).unwrap();
    assert!(
        matches!(Journal::open(&profile), Err(JournalError::ChainBroken { seq: 2 })),
        "an edited payload must be detected, and at the row that was edited"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_deleted_event_breaks_the_chain_on_reopen() {
    // The reason prev_signature is mixed into the MAC. Without chaining, DELETE would be an
    // undetectable way to forget -- and forgetting must remove accessibility, never
    // availability of the record that it happened.
    let dir = tmp("delete");
    // Scoped so the profile's exclusivity lock releases before the reopen below. Two live
    // `Profile` values on one root is the configuration that forks the journal's hash chain.
    {
        let profile = Profile::init(&dir).unwrap();
        let mut journal = Journal::open(&profile).unwrap();
        let clock = Clock::new(1_780_000_000_000);
        for i in 0..3 {
            journal
                .append(
                    clock,
                    req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({"i": i})),
                )
                .unwrap();
        }
    }

    let conn = rusqlite::Connection::open(dir.join("journal.db")).unwrap();
    conn.execute("DELETE FROM journal WHERE seq = 2", []).unwrap();
    drop(conn);

    let profile = Profile::open(&dir).unwrap();
    match Journal::open(&profile) {
        Err(JournalError::SequenceGap { expected: 2, found: 3 }) => {}
        Err(other) => panic!("a deleted event must be detected as a gap, got {other:?}"),
        Ok(_) => panic!("a deleted event went undetected; DELETE would be a way to forget"),
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reopening_an_intact_journal_verifies_and_resumes() {
    let dir = tmp("resume");
    // Scoped so the profile's exclusivity lock releases before the reopen below. Two live
    // `Profile` values on one root is the configuration that forks the journal's hash chain.
    {
        let profile = Profile::init(&dir).unwrap();
        let mut journal = Journal::open(&profile).unwrap();
        let clock = Clock::new(1_780_000_000_000);
        for i in 0..4 {
            journal
                .append(
                    clock,
                    req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({"i": i})),
                )
                .unwrap();
        }
    }

    let profile = Profile::open(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    assert_eq!(journal.last_seq(), 4, "verification must recover the tail");

    // And appending continues the same chain rather than starting a new one.
    let next = journal
        .append(
            Clock::new(1_780_000_100_000),
            req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({"i": 4})),
        )
        .unwrap();
    assert_eq!(next.seq, 5);
    // Both the journal and the profile go, so the lock releases before the final reopen.
    drop(journal);
    drop(profile);

    let profile = Profile::open(&dir).unwrap();
    assert!(Journal::open(&profile).is_ok(), "the chain must still verify");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn replay_reads_back_what_was_appended() {
    let dir = tmp("replay");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let clock = Clock::new(1_780_000_000_000);

    journal
        .append(clock, req(EventKind::MemoryWritten, Actor::Harness, serde_json::json!({"a": 1})))
        .unwrap();
    journal
        .append(clock, req(EventKind::Tombstoned, Actor::Harness, serde_json::json!({"b": 2})))
        .unwrap();

    // The capability is the point: this call does not compile without one, and grepping for
    // the constructor enumerates every place replay is reachable from.
    let cap = OperatorCapability::for_operator_or_audit();
    let all = journal.replay(&cap, None).unwrap();
    assert_eq!(all.len(), 2);

    let writes = journal.replay(&cap, Some(EventKind::MemoryWritten)).unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].2, serde_json::json!({"a": 1}));
    let _ = fs::remove_dir_all(&dir);
}
