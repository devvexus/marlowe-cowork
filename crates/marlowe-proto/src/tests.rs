//! Wire round-trips and framing.

use std::io::{BufReader, Cursor};

use marlowe_model::{Effort, ModelInfo, ModelRef, Part, ProviderKind, Role, Usage};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

fn sid() -> SessionId {
    SessionId("s1".into())
}

fn mref() -> ModelRef {
    "ollama/qwen3:8b".parse().unwrap()
}

#[test]
fn every_request_round_trips_and_is_tagged_by_op() {
    let reqs = vec![
        Request::Hello { token: "t".into(), client: "tui".into(), since_seq: Some(4) },
        Request::Ask { session: None, text: "hi".into(), workspace: Some("C:/w".into()) },
        Request::Interrupt { session: sid() },
        Request::ListSessions { limit: 20 },
        Request::OpenSession { session: sid() },
        Request::NewSession { workspace: None },
        Request::Approve { approval: ApprovalId(3), decision: ApprovalDecision::AllowForSession },
        Request::LibraryState,
        Request::DetectProviders,
        Request::ListCatalog { provider: ProviderKind::OpenRouter },
        Request::AddModel { provider: ProviderKind::Ollama, model_id: "qwen3:8b".into() },
        Request::RemoveModel { model: mref() },
        Request::SetSlot { slot: Slot::Crew("researcher".into()), model: Some(mref()), effort: Effort::High },
        Request::SetCredential { provider: ProviderKind::OpenRouter, secret: Secret("sk-or-xyz".into()) },
        Request::Status,
        Request::Shutdown,
    ];
    for r in &reqs {
        let s = round_trip(r);
        assert!(s.starts_with(r#"{"op":""#), "{s}");
    }
    let s = serde_json::to_string(&reqs[12]).unwrap();
    assert!(s.contains(r#""slot":"crew:researcher""#), "{s}");
    assert!(s.contains(r#""model":"ollama/qwen3:8b""#), "{s}");
    let s = serde_json::to_string(&reqs[6]).unwrap();
    assert!(s.contains(r#""decision":"allow_for_session""#), "{s}");
}

#[test]
fn a_secret_never_appears_in_debug_output() {
    let r = Request::SetCredential { provider: ProviderKind::OpenRouter, secret: Secret("sk-or-xyz".into()) };
    let dbg = format!("{r:?}");
    assert!(!dbg.contains("sk-or-xyz"), "{dbg}");
    assert!(serde_json::to_string(&r).unwrap().contains("sk-or-xyz"), "the wire still carries it");
}

#[test]
fn minimal_requests_parse_with_defaults() {
    let r: Request = serde_json::from_str(r#"{"op":"ask","text":"hello"}"#).unwrap();
    assert_eq!(r, Request::Ask { session: None, text: "hello".into(), workspace: None });
    let r: Request = serde_json::from_str(r#"{"op":"set_slot","slot":"quick"}"#).unwrap();
    assert_eq!(r, Request::SetSlot { slot: Slot::Quick, model: None, effort: Effort::Medium });
    assert!(serde_json::from_str::<Request>(r#"{"op":"set_slot","slot":"chef"}"#).is_err());
}

#[test]
fn every_event_round_trips_inside_an_envelope() {
    let info = ModelInfo::bare(ProviderKind::Ollama, "qwen3:8b");
    let events = vec![
        Event::Welcome { daemon_version: "0.1.0".into(), proto_version: PROTO_VERSION, profile_home: "C:/h".into(), seq: 9 },
        Event::SessionOpened { session: sid(), title: "t".into(), workspace: None },
        Event::SessionList {
            sessions: vec![SessionSummary {
                id: sid(),
                title: "t".into(),
                workspace: Some("w".into()),
                created_ms: 1,
                updated_ms: 2,
                turns: 3,
            }],
        },
        Event::Transcript {
            session: sid(),
            turns: vec![StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "hi".into() }], at_ms: 5 }],
        },
        Event::TurnStarted { session: sid(), turn: 1, model: mref(), user: "hi".into() },
        Event::TextDelta { session: sid(), turn: 1, text: "Hel".into() },
        Event::ReasoningDelta { session: sid(), turn: 1, text: "hm".into(), tokens: 1 },
        Event::TextRetracted { session: sid(), turn: 1, tokens: 2 },
        Event::ToolStarted { session: sid(), turn: 1, call_id: "c".into(), tool: "read".into(), summary: "src/main.rs".into() },
        Event::ToolFinished { session: sid(), turn: 1, call_id: "c".into(), ok: true, summary: "41 lines".into() },
        Event::TurnFinished { session: sid(), turn: 1, usage: Usage { input: 3, ..Default::default() }, stop: TurnStop::Done },
        Event::ApprovalRequested { approval: ApprovalId(1), session: sid(), tool: "bash".into(), summary: "cargo test".into(), detail: "{}".into() },
        Event::ApprovalResolved { approval: ApprovalId(1), decision: ApprovalDecision::Deny },
        Event::LibraryState {
            models: vec![LibraryModel { model: mref(), info: info.clone(), added_ms: 7 }],
            slots: vec![SlotAssignment { slot: Slot::Conductor, model: Some(mref()), effort: Effort::Low, inherited: true }],
            providers: vec![ProviderStatus {
                kind: ProviderKind::Ollama,
                available_now: true,
                configured: true,
                detected: true,
                note: Some("running".into()),
            }],
        },
        Event::Catalog { provider: ProviderKind::Ollama, models: vec![info] },
        Event::Status(StatusReport {
            daemon_version: "0.1.0".into(),
            pid: 42,
            uptime_ms: 10,
            clients: 1,
            active_sessions: vec![sid()],
            seq: 3,
            profile_home: "h".into(),
        }),
        Event::Error { session: None, message: "nope".into() },
        Event::ShuttingDown,
    ];
    for (i, ev) in events.into_iter().enumerate() {
        let env = Envelope { seq: i as u64, at_ms: 1_700_000_000_000, event: ev };
        let s = round_trip(&env);
        assert!(s.contains(r#""ev":""#), "{s}");
    }
}

#[test]
fn events_know_their_session() {
    assert_eq!(Event::TextDelta { session: sid(), turn: 0, text: String::new() }.session(), Some(&sid()));
    assert_eq!(Event::ShuttingDown.session(), None);
}

#[test]
fn slots_parse_and_print() {
    for (s, slot) in [
        ("conductor", Slot::Conductor),
        ("crew:researcher", Slot::Crew("researcher".into())),
        ("reflection", Slot::Reflection),
        ("quick", Slot::Quick),
    ] {
        assert_eq!(s.parse::<Slot>().unwrap(), slot);
        assert_eq!(slot.to_string(), s);
    }
    assert_eq!("Crew: engineer".parse::<Slot>().unwrap(), Slot::Crew("engineer".into()));
    assert!("crew:".parse::<Slot>().is_err());
    assert!("pilot".parse::<Slot>().is_err());
}

#[test]
fn frames_round_trip_as_json_lines() {
    let mut out = Vec::new();
    write_line(&mut out, &Request::Status).unwrap();
    write_line(&mut out, &Request::ListSessions { limit: 5 }).unwrap();
    out.extend_from_slice(b"\r\n   \n");
    assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 4);
    let mut r = LineReader::new(BufReader::new(Cursor::new(out)));
    assert_eq!(r.next::<Request>().unwrap(), Some(Request::Status));
    assert_eq!(r.next::<Request>().unwrap(), Some(Request::ListSessions { limit: 5 }));
    assert_eq!(r.next::<Request>().unwrap(), None, "blank lines are skipped");
}

#[test]
fn a_bad_line_is_reported_and_the_stream_continues() {
    let data = b"{not json}\n{\"op\":\"status\"}\n".to_vec();
    let mut r = LineReader::new(BufReader::new(Cursor::new(data)));
    let e = r.next::<Request>().unwrap_err();
    assert!(!e.is_fatal());
    assert_eq!(r.next::<Request>().unwrap(), Some(Request::Status));
}

#[test]
fn an_oversized_line_is_refused_without_buffering_it() {
    let mut data = vec![b'x'; 100];
    data.push(b'\n');
    data.extend_from_slice(b"{\"op\":\"shutdown\"}\n");
    // A tiny BufReader capacity forces the line across many fill_buf calls.
    let mut r = LineReader::with_limit(BufReader::with_capacity(8, Cursor::new(data)), 32);
    assert!(matches!(r.next::<Request>(), Err(FrameError::TooLong)));
    assert_eq!(r.next::<Request>().unwrap(), Some(Request::Shutdown));
}

#[test]
fn a_final_line_without_newline_is_still_read() {
    let mut r = LineReader::new(BufReader::new(Cursor::new(b"{\"op\":\"status\"}".to_vec())));
    assert_eq!(r.next::<Request>().unwrap(), Some(Request::Status));
    assert_eq!(r.next::<Request>().unwrap(), None);
}

/// Yields its chunks in order, with a `TimedOut` error between each.
struct Stuttering {
    chunks: Vec<&'static [u8]>,
    timed_out: bool,
}

impl std::io::Read for Stuttering {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if !self.timed_out && !self.chunks.is_empty() {
            self.timed_out = true;
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.timed_out = false;
        let Some(c) = (!self.chunks.is_empty()).then(|| self.chunks.remove(0)) else { return Ok(0) };
        out[..c.len()].copy_from_slice(c);
        Ok(c.len())
    }
}

#[test]
fn a_timeout_mid_line_keeps_the_partial_line() {
    let src = Stuttering { chunks: vec![b"{\"op\":", b"\"status\"}\n{\"op\":\"shu", b"tdown\"}\n"], timed_out: false };
    let mut r = LineReader::new(BufReader::new(src));
    let mut got = Vec::new();
    for _ in 0..20 {
        match r.next::<Request>() {
            Ok(Some(req)) => got.push(req),
            Ok(None) => break,
            Err(FrameError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, vec![Request::Status, Request::Shutdown]);
}

#[test]
fn daemon_info_writes_reads_and_removes() {
    let home = std::env::temp_dir().join(format!("marlowe-proto-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(DaemonInfo::read(&home).unwrap(), None);
    let info = DaemonInfo { port: 4567, token: auth::mint_token().unwrap(), pid: 99 };
    info.write(&home).unwrap();
    assert_eq!(DaemonInfo::read(&home).unwrap(), Some(info.clone()));
    assert_eq!(info.addr().to_string(), "127.0.0.1:4567");
    DaemonInfo::remove_if_ours(&home, 1).unwrap();
    assert!(DaemonInfo::read(&home).unwrap().is_some(), "another pid's file is left alone");
    DaemonInfo::remove_if_ours(&home, 99).unwrap();
    assert_eq!(DaemonInfo::read(&home).unwrap(), None);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn session_ids_are_random_hex() {
    let a = SessionId::new_random();
    assert_eq!(a.as_str().len(), 32);
    assert_ne!(a, SessionId::new_random());
    assert_eq!(serde_json::to_string(&SessionId("x".into())).unwrap(), r#""x""#);
}

fn hit() -> MemoryHit {
    MemoryHit {
        id: "m-s1-t1-0".into(),
        text: "the deploy job runs on Fridays".into(),
        kind: MemoryKind::Decision,
        trust: MemoryTrust::User,
        scope: MemoryScope::Project("C:/w".into()),
        occurred_at_ms: 1_700_000_000_000,
        provenance: "you, session s1".into(),
        score: 0.75,
    }
}

#[test]
fn memory_requests_round_trip_and_parse_with_defaults() {
    let reqs = vec![
        Request::MemorySearch { query: "deploy".into(), scope: Some(MemoryScope::Global), limit: 10 },
        Request::MemorySearch { query: "deploy".into(), scope: Some(MemoryScope::Project("C:/w".into())), limit: 3 },
        Request::MemorySearch { query: "deploy".into(), scope: None, limit: 10 },
        Request::MemoryForget { id: "m-1".into() },
        Request::MemoryCorrect { id: "m-1".into(), text: "Thursdays".into() },
        Request::MemoryStats,
        Request::ImportLegacy { path: Some("C:/old".into()) },
        Request::ImportLegacy { path: None },
        Request::LegacyStatus,
        Request::MemoryProbe { query: "deploy".into(), workspace: Some("C:/w".into()) },
        Request::MemoryProbe { query: "deploy".into(), workspace: None },
    ];
    for r in &reqs {
        let s = round_trip(r);
        assert!(s.starts_with(r#"{"op":""#), "{s}");
    }
    let s = serde_json::to_string(&reqs[1]).unwrap();
    assert!(s.contains(r#""op":"memory_search""#) && s.contains(r#""scope":{"project":"C:/w"}"#), "{s}");
    let s = serde_json::to_string(&reqs[0]).unwrap();
    assert!(s.contains(r#""scope":"global""#), "{s}");
    let r: Request = serde_json::from_str(r#"{"op":"memory_search","query":"q","limit":5}"#).unwrap();
    assert_eq!(r, Request::MemorySearch { query: "q".into(), scope: None, limit: 5 });
    let r: Request = serde_json::from_str(r#"{"op":"import_legacy"}"#).unwrap();
    assert_eq!(r, Request::ImportLegacy { path: None });
    let r: Request = serde_json::from_str(r#"{"op":"legacy_status"}"#).unwrap();
    assert_eq!(r, Request::LegacyStatus);
}

#[test]
fn memory_events_round_trip_inside_an_envelope() {
    let report = ImportReport {
        read: 120,
        imported: 100,
        skipped_duplicates: 15,
        tombstoned: 3,
        superseded: 2,
        errors: vec!["seq 7: bad payload".into()],
        conversation_turns_read: 40,
        conversation_turns_imported: 30,
        conversation_user_turns: 12,
        conversation_user_requests: 7,
        conversation_assistant_turns: 18,
        conversation_skipped: 4,
    };
    let events = vec![
        Event::MemoryResults { hits: vec![hit()] },
        Event::MemoryResults { hits: Vec::new() },
        Event::MemoryChanged { id: "m-1".into(), change: MemoryChange::Remembered },
        Event::MemoryChanged { id: "m-1".into(), change: MemoryChange::Corrected { by: "m-2".into() } },
        Event::MemoryChanged { id: "m-1".into(), change: MemoryChange::Forgotten },
        Event::MemoryStats(MemoryStatsReport {
            beliefs: 10,
            episodes: 2,
            vectors: 9,
            models_loaded: vec!["jina-embeddings-v2-small-en".into()],
            degraded: None,
            journal_seq: 44,
        }),
        Event::MemoryStats(MemoryStatsReport { degraded: Some("models missing".into()), ..Default::default() }),
        Event::Recalled { session: sid(), turn: 2, count: 3, summary: "deploy on Fridays".into() },
        Event::ImportProgress { read: 50, imported: 40 },
        Event::ImportDone { report },
        Event::LegacyAvailable { path: "C:/old".into(), approx_entries: 1200, already_imported: false },
        Event::MemoryProbed { recalled: vec![hit()], abstained: None, latency_ms: 41 },
        Event::MemoryProbed { recalled: Vec::new(), abstained: Some("no runner-up".into()), latency_ms: 0 },
    ];
    for (i, ev) in events.into_iter().enumerate() {
        let env = Envelope { seq: i as u64, at_ms: 1_700_000_000_000, event: ev };
        let s = round_trip(&env);
        assert!(s.contains(r#""ev":""#), "{s}");
    }
    let s = serde_json::to_string(&Event::MemoryChanged { id: "m-1".into(), change: MemoryChange::Corrected { by: "m-2".into() } }).unwrap();
    assert!(s.contains(r#""change":{"kind":"corrected","by":"m-2"}"#), "{s}");
    let ev: Event = serde_json::from_str(r#"{"ev":"legacy_available","path":"p","approx_entries":3}"#).unwrap();
    assert_eq!(ev, Event::LegacyAvailable { path: "p".into(), approx_entries: 3, already_imported: false });
    // A report from before the conversation counts still decodes.
    let ev: Event = serde_json::from_str(
        r#"{"ev":"import_done","report":{"read":3,"imported":2,"skipped_duplicates":1,"tombstoned":0,"superseded":0}}"#,
    )
    .unwrap();
    let old = ImportReport { read: 3, imported: 2, skipped_duplicates: 1, ..ImportReport::default() };
    assert_eq!(ev, Event::ImportDone { report: old });
    let ev: Event = serde_json::from_str(r#"{"ev":"memory_stats","beliefs":1,"episodes":0,"vectors":0,"journal_seq":5}"#).unwrap();
    assert_eq!(ev, Event::MemoryStats(MemoryStatsReport { beliefs: 1, journal_seq: 5, ..Default::default() }));
}

#[test]
fn recalled_belongs_to_its_session_and_memory_events_to_none() {
    assert_eq!(Event::Recalled { session: sid(), turn: 1, count: 1, summary: String::new() }.session(), Some(&sid()));
    assert_eq!(Event::MemoryResults { hits: vec![hit()] }.session(), None);
}

#[test]
fn memory_kinds_and_trust_have_stable_labels() {
    for k in MemoryKind::ALL {
        assert_eq!(MemoryKind::from_label(k.label()), Some(k));
        assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{}\"", k.label()));
    }
    assert_eq!(MemoryKind::from_label(" Decision "), Some(MemoryKind::Decision));
    assert_eq!(MemoryKind::from_label("rumour"), None);
    assert!(MemoryTrust::Untrusted < MemoryTrust::Inferred && MemoryTrust::Observed < MemoryTrust::User);
    assert_eq!(serde_json::to_string(&MemoryTrust::User).unwrap(), r#""user""#);
}
