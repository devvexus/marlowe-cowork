//! A snapshot plus the journal tail must derive exactly what a full replay derives.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::{Channel, Clock, IngestRequest, Origin, PayloadKind, Speaker, TrustClass, Turn, CONTRACT_VERSION};
use marlowe_journal::{EventKind, Journal, OperatorCapability, Profile, SigningKey, DERIVATION_VERSION};
use marlowe_memory::cue::lexical::score_indexed;
use marlowe_memory::{
    correct_belief, correct_claim, forget_claim, ingest, remember_claim, write_belief, Author,
    BeliefKind, BeliefScope, BeliefSnapshot, BeliefStore, BeliefWrite, ClaimWrite,
    MaturationPolicy, MemoryEntry, MemoryError, Tags, Utterance,
};

const T0: i64 = 1_780_000_000_000;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-snapshot-it-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn turn(id: &str, text: &str, at: i64) -> Turn {
    Turn {
        turn_id: id.into(),
        speaker: Speaker::User,
        text: text.into(),
        occurred_at_ms: at,
        origin: Origin { channel: Channel::Terminal, actor: "user:primary".into(), r#ref: None },
    }
}

fn tagged<'a>(session: &'a str, turn_id: &'a str, text: &'a str, author: Author, tags: Tags) -> BeliefWrite<'a> {
    BeliefWrite {
        session_id: session,
        turn_id,
        run_id: None,
        text,
        author,
        derived_from: &[],
        occurred_at_ms: T0,
        tags,
        maturation: MaturationPolicy::ByTrust,
    }
}

fn kind(k: BeliefKind) -> Tags {
    Tags { kind: Some(k), ..Tags::default() }
}

/// Phase one of the history: everything a snapshot will cover.
fn phase_one(journal: &mut Journal, beliefs: &mut BeliefStore) -> Vec<String> {
    let clock = Clock::new(T0);
    ingest(
        journal,
        beliefs,
        &IngestRequest {
            contract_version: CONTRACT_VERSION.to_string(),
            clock,
            session_id: "eval-s".into(),
            turns: vec![
                turn("t0", "we moved off Postgres in April 2023", T0 - 5_000),
                turn("t1", "the nightly ingest job times out", T0 - 4_000),
            ],
        },
    )
    .unwrap();

    let mut ids = Vec::new();
    let pref = write_belief(
        journal,
        beliefs,
        clock,
        &tagged("tui", "turn-1", "Matthew prefers tabs over spaces", Author::User, kind(BeliefKind::Preference)),
    )
    .unwrap()
    .unwrap();
    ids.push(pref.id);

    let scoped = Tags {
        kind: Some(BeliefKind::Decision),
        scope: BeliefScope::Project(r"C:\Users\m\proj".into()),
        reason: Some("the eval needs determinism".into()),
        ..Tags::default()
    };
    let decision = write_belief(
        journal,
        beliefs,
        clock,
        &tagged("tui", "turn-2", "use BTreeMap on the scored path", Author::Agent { run_floor: TrustClass::UserAsserted }, scoped),
    )
    .unwrap()
    .unwrap();
    ids.push(decision.id);

    let prose = Tags { kind: Some(BeliefKind::Episode), utterance: Some(Utterance::Assistant), ..Tags::default() };
    let reply = write_belief(
        journal,
        beliefs,
        clock,
        &tagged("tui", "turn-2", "Sure, I switched it to a BTreeMap.", Author::Agent { run_floor: TrustClass::UserAsserted }, prose),
    )
    .unwrap()
    .unwrap();
    ids.push(reply.id);

    let imported = Tags {
        kind: Some(BeliefKind::Fact),
        source: Some("imported from Marlowe_Harness".into()),
        ..Tags::default()
    };
    let old = write_belief(
        journal,
        beliefs,
        clock,
        &tagged(
            "import",
            "import-7",
            "the deploy job runs on Fridays",
            Author::Imported { trust_class: TrustClass::UserAsserted, effective_trust: TrustClass::UserAsserted },
            imported,
        ),
    )
    .unwrap()
    .unwrap();
    ids.push(old.id);

    let claim = remember_claim(
        journal,
        beliefs,
        clock,
        &ClaimWrite {
            session_id: "tui",
            run_id: "run-1",
            text: "the cache lives under models/",
            payload_kind: PayloadKind::Fact,
            derived_from: &[],
            run_floor: TrustClass::UserAsserted,
        },
    )
    .unwrap()
    .unwrap();
    ids.push(claim.id);
    ids
}

/// Phase two: events after the watermark, several of which reach back into phase one.
fn phase_two(journal: &mut Journal, beliefs: &mut BeliefStore, earlier: &[String]) {
    let clock = Clock::new(T0 + 60_000);
    // A user correction of the agent's decision (pre-watermark target).
    correct_belief(
        journal,
        beliefs,
        clock,
        &tagged("tui", "turn-3", "use BTreeMap everywhere, not just the scored path", Author::User, kind(BeliefKind::Decision)),
        &earlier[1],
    )
    .unwrap()
    .unwrap();
    // Forget the imported fact (pre-watermark target).
    forget_claim(journal, beliefs, clock, "tui", "run-2", &earlier[3]).unwrap().unwrap();
    // A claim correction across the watermark.
    correct_claim(
        journal,
        beliefs,
        clock,
        &ClaimWrite {
            session_id: "tui",
            run_id: "run-2",
            text: "the cache lives under models/.cache",
            payload_kind: PayloadKind::Fact,
            derived_from: &[],
            run_floor: TrustClass::UserAsserted,
        },
        &earlier[4],
    )
    .unwrap()
    .unwrap();
    // A new tagged write and a refused one (journalled, not a belief).
    write_belief(
        journal,
        beliefs,
        clock,
        &tagged("tui", "turn-4", "results: R@1 0.6725 held out", Author::Harness, kind(BeliefKind::Result)),
    )
    .unwrap()
    .unwrap();
    let refused = write_belief(journal, beliefs, clock, &tagged("tui", "turn-5", "   ", Author::User, Tags::default())).unwrap();
    assert!(refused.is_err());
}

fn entries(store: &BeliefStore) -> Vec<MemoryEntry> {
    store.entries().cloned().collect()
}

fn lexical_fingerprint(store: &BeliefStore) -> Vec<Vec<u32>> {
    let docs: Vec<&MemoryEntry> = store.entries().collect();
    ["postgres april", "btreemap scored path", "cache models", "fridays deploy", ""]
        .iter()
        .map(|q| score_indexed(store.lexicon(), &docs, q).iter().map(|s| s.to_bits()).collect())
        .collect()
}

#[test]
fn snapshot_plus_tail_equals_a_full_derive() {
    let dir = tmp("equal");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut live = BeliefStore::default();

    let earlier = phase_one(&mut journal, &mut live);
    let watermark = journal.last_seq();
    let sealed = live.snapshot(watermark).seal(profile.key()).unwrap();
    phase_two(&mut journal, &mut live, &earlier);
    assert!(journal.last_seq() > watermark + 4, "the tail is not trivial");

    let full = BeliefStore::derive(&journal, DERIVATION_VERSION).unwrap();
    let snap = BeliefSnapshot::open(&sealed, profile.key()).unwrap();
    assert_eq!(snap.journal_seq, watermark);
    let booted = BeliefStore::from_snapshot(snap, &journal, DERIVATION_VERSION).unwrap();

    assert_eq!(entries(&booted), entries(&full), "snapshot + tail must equal a full derive");
    assert_eq!(entries(&live), entries(&full), "and the live store must equal both");
    assert_eq!(lexical_fingerprint(&booted), lexical_fingerprint(&full));
    assert_eq!(lexical_fingerprint(&live), lexical_fingerprint(&full));

    // Controls: the tail really did reach back into the snapshot's entries.
    let decision = full.get(&earlier[1]).unwrap();
    assert!(decision.superseded_by.is_some(), "corrected after the watermark");
    assert_eq!(decision.scope(), &BeliefScope::Project(r"C:\Users\m\proj".into()));
    assert_eq!(decision.tags.reason.as_deref(), Some("the eval needs determinism"));
    let forgotten = full.get(&earlier[3]).unwrap();
    assert!(forgotten.text.is_empty(), "tombstoned after the watermark");
    assert!(full.get(&earlier[2]).unwrap().is_assistant_prose());

    // An empty snapshot at seq 0 is a full derive by another name.
    let empty = BeliefStore::default().snapshot(0);
    let from_zero = BeliefStore::from_snapshot(empty, &journal, DERIVATION_VERSION).unwrap();
    assert_eq!(entries(&from_zero), entries(&full));

    drop(journal);
    drop(profile);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_snapshot_is_refused_rather_than_trusted_when_anything_is_off() {
    let dir = tmp("refuse");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut live = BeliefStore::default();
    phase_one(&mut journal, &mut live);
    let seq = journal.last_seq();
    let sealed = live.snapshot(seq).seal(profile.key()).unwrap();

    // Another profile's key.
    let other = SigningKey::from_bytes([7u8; 32]);
    assert!(matches!(BeliefSnapshot::open(&sealed, &other), Err(MemoryError::SnapshotRejected(_))));

    // One flipped byte in the body.
    let mut tampered = sealed.clone();
    let last = tampered.len() - 2;
    tampered[last] ^= 0x01;
    assert!(matches!(BeliefSnapshot::open(&tampered, profile.key()), Err(MemoryError::SnapshotRejected(_))));

    // Not a snapshot at all, and a truncated one.
    assert!(BeliefSnapshot::open(b"{}", profile.key()).is_err());
    assert!(BeliefSnapshot::open(&sealed[..40], profile.key()).is_err());

    // A watermark the journal has not reached.
    let ahead = live.snapshot(seq + 10);
    assert!(matches!(
        BeliefStore::from_snapshot(ahead, &journal, DERIVATION_VERSION),
        Err(MemoryError::SnapshotRejected(_))
    ));

    // Another derivation.
    let mut stale = live.snapshot(seq);
    stale.derivation_version = DERIVATION_VERSION - 1;
    assert!(matches!(
        BeliefStore::from_snapshot(stale, &journal, DERIVATION_VERSION),
        Err(MemoryError::SnapshotRejected(_))
    ));

    // The good one still opens.
    let ok = BeliefSnapshot::open(&sealed, profile.key()).unwrap();
    assert_eq!(ok.entries.len(), live.len());

    drop(journal);
    drop(profile);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn untagged_writes_serialize_exactly_as_before_and_tagged_ones_carry_their_tags() {
    let dir = tmp("bytes");
    let profile = Profile::init(&dir).unwrap();
    let mut journal = Journal::open(&profile).unwrap();
    let mut live = BeliefStore::default();
    phase_one(&mut journal, &mut live);

    let cap = OperatorCapability::for_operator_or_audit();
    let written = journal.replay(&cap, Some(EventKind::MemoryWritten)).unwrap();
    let keys = |v: &serde_json::Value| {
        let mut k = v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        k.sort();
        k
    };
    // The two ingest turns and the claim: the eval's event shape, with no new key.
    let untagged: Vec<_> = written.iter().filter(|(_, _, p)| p.get("tags").is_none()).collect();
    assert_eq!(untagged.len(), 3);
    for (_, _, p) in &untagged {
        assert_eq!(
            keys(p),
            vec![
                "created_at", "derivation", "effective_trust", "fidelity", "id", "occurred_at_ms",
                "payload_kind", "silent_until", "source_session_id", "source_turn_id", "text",
                "trust_class",
            ]
        );
    }
    let tagged = written.iter().filter(|(_, _, p)| p.get("tags").is_some()).count();
    assert_eq!(tagged, 4);

    drop(journal);
    drop(profile);
    let _ = fs::remove_dir_all(&dir);
}
