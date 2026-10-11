//! **Memory survives a restart; the conversation does not.** M2 Session D.
//!
//! The claim is cheap to state and easy to get wrong in the direction that looks fine: a daemon
//! that silently started with an empty store would behave exactly like a first run. So the
//! durability assertion here carries its own control — a *different* profile root must come back
//! empty, or the test would pass on a build that returned the same store to everyone.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_daemon::memory::DaemonMemory;
use marlowe_journal::{Journal, Profile};
use marlowe_loop::driver::{ClaimRequest, MemoryHost};
use marlowe_loop::run::{RunId, SessionId};
use marlowe_memory::DERIVATION_VERSION;

const T0: i64 = 1_780_000_000_000;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-daemon-mem-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Open a memory over a profile root, initialising it if this is the first time — the same
/// existence check `Daemon::open` makes.
fn open(root: &PathBuf) -> DaemonMemory {
    let profile = if root.join("profile.json").exists() {
        Profile::open(root).unwrap()
    } else {
        Profile::init(root).unwrap()
    };
    let journal = Journal::open(&profile).unwrap();
    // **No reranking directory: write-only.** These tests are about the WRITE path, and loading a
    // 60 MB graph to exercise it would make them slow and make a missing model look like a write
    // failure. `retrieval_is_announced_as_write_only_when_no_graph_is_loaded` asserts that this
    // state is reported rather than silent.
    DaemonMemory::open(
        Arc::new(Mutex::new(journal)),
        DERIVATION_VERSION,
        None,
        "test-model",
        // Stated rather than defaulted: ADR-060 made this a required field precisely so a call
        // site cannot mean Ollama by omission. This fixture reserves nothing either way -- there
        // is no cross-encoder here -- so `Ollama` is the honest label for what it is standing in
        // for, and it is the branch that would shell out if anything did read it.
        marlowe_memory::cue::dense::vram::Tier1Runtime::Ollama,
    )
        .unwrap()
}

fn claim(text: &str) -> ClaimRequest {
    ClaimRequest {
        text: text.to_string(),
        payload_kind: String::new(),
        derived_from: Vec::new(),
    }
}

/// **A daemon with no cross-encoder is write-only, and says so.**
///
/// The failure this closes is silence. Without an announcement, a daemon whose retrieval half never
/// loaded behaves *identically* to one whose store is empty — Marlowe simply does not remember, and
/// there is nothing to distinguish "the model is missing" from "you never told me". ADR-029's
/// announce-never-infer rule, applied to memory.
#[test]
fn retrieval_is_announced_as_write_only_when_no_graph_is_loaded() {
    let root = tmp("announce");
    let memory = open(&root);
    match memory.state() {
        marlowe_daemon::memory::RetrievalState::WriteOnly { why } => {
            assert!(
                why.contains("margin") || why.contains("reranking"),
                "the reason must name what is missing, not merely that something is: {why}"
            );
        }
        other => panic!("a daemon with no reranking directory must be write-only, got {other:?}"),
    }
    assert!(
        memory.state().headline().contains("WRITE-ONLY"),
        "and the headline must be legible at a glance: {}",
        memory.state().headline()
    );
    let _ = fs::remove_dir_all(&root);
}

/// The write path is unaffected by retrieval being off — memory still records.
#[test]
fn a_write_only_daemon_still_remembers() {
    let root = tmp("write-only-writes");
    let mut memory = open(&root);
    let receipt = memory
        .remember(
            RunId::new(),
            SessionId::from_name("s"),
            &claim("a fact"),
            TrustClass::UserAsserted,
            T0,
        )
        .expect("writing does not need a reranker");
    assert!(receipt.contains("AgentInferred"));
    assert_eq!(memory.len(), 1);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_claim_written_today_is_still_there_after_the_daemon_restarts() {
    let root = tmp("durable");
    let other = tmp("durable-control");

    // ── the first "process" ──────────────────────────────────────────────────────
    {
        let mut memory = open(&root);
        assert!(memory.is_empty(), "a fresh profile starts with nothing");

        let receipt = memory
            .remember(
                RunId::new(),
                SessionId::from_name("tui"),
                &claim("the release train leaves on Thursday mornings"),
                TrustClass::UserAsserted,
                T0,
            )
            .expect("the claim should be accepted");
        assert!(receipt.contains("AgentInferred"), "a clean run writes at AgentInferred: {receipt}");
        assert_eq!(memory.len(), 1);
    }

    // ── the daemon stops, and a new one opens the same profile ───────────────────
    let reopened = open(&root);
    assert_eq!(
        reopened.len(),
        1,
        "the belief store is a materialized view over the journal, so a restart rebuilds it. If \
         this is 0 the write never reached the log, or the fold dropped it"
    );

    // ── the control ──────────────────────────────────────────────────────────────
    //
    // Without this the assertion above passes on a build that hands every caller the same store,
    // or one that ignores the profile root entirely.
    let unrelated = open(&other);
    assert!(
        unrelated.is_empty(),
        "a different profile root must not see the first one's beliefs; if it does, the store is \
         not keyed to the log it was derived from"
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&other);
}

/// ADR-038 end to end through the daemon's host, both directions in one test.
#[test]
fn the_run_floor_reaches_the_daemons_write_path() {
    let root = tmp("floor");
    let mut memory = open(&root);

    let clean = memory
        .remember(
            RunId::new(),
            SessionId::from_name("clean"),
            &claim("a fact learned in a clean run"),
            TrustClass::UserAsserted,
            T0,
        )
        .unwrap();

    let tainted = memory
        .remember(
            RunId::new(),
            SessionId::from_name("dirty"),
            &claim("a fact learned after reading a page"),
            TrustClass::UntrustedContent,
            T0,
        )
        .unwrap();

    assert!(clean.contains("AgentInferred"), "{clean}");
    assert!(tainted.contains("UntrustedContent"), "{tainted}");
    assert_ne!(
        clean.contains("UntrustedContent"),
        tainted.contains("UntrustedContent"),
        "identical shape, different floor — if these agree the floor is not reaching the host"
    );

    let _ = fs::remove_dir_all(&root);
}

/// An unknown `payload_kind` is refused rather than quietly reclassified.
///
/// The same rule §4.6 applies to `channel`: a default arm would let the model write a belief under
/// a kind nobody chose, and the kind is what a later filter reads.
#[test]
fn an_unrecognised_payload_kind_is_refused_and_an_absent_one_is_not() {
    let root = tmp("kind");
    let mut memory = open(&root);

    let bad = memory.remember(
        RunId::new(),
        SessionId::from_name("s"),
        &ClaimRequest {
            text: "a claim".into(),
            payload_kind: "rumour".into(),
            derived_from: Vec::new(),
        },
        TrustClass::UserAsserted,
        T0,
    );
    assert!(bad.is_err(), "an unknown kind must not be defaulted: {bad:?}");
    assert!(memory.is_empty(), "and nothing may have been written");

    // The control: omitting the optional argument is a legitimate call and must still succeed, or
    // the refusal above would just be "this path never works".
    let good = memory.remember(
        RunId::new(),
        SessionId::from_name("s"),
        &claim("a claim"),
        TrustClass::UserAsserted,
        T0,
    );
    assert!(good.is_ok(), "an omitted payload_kind is legitimate: {good:?}");
    assert_eq!(memory.len(), 1);

    let _ = fs::remove_dir_all(&root);
}
