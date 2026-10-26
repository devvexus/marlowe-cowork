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
        Request::Ask { session: None, text: "hi".into(), workspace: Some("C:/w".into()), images: Vec::new(), options: None, client_ms: None },
        Request::Interrupt { session: sid() },
        Request::ListSessions { limit: 20 },
        Request::OpenSession { session: sid() },
        Request::NewSession { workspace: None },
        Request::Approve { approval: ApprovalId(3), decision: ApprovalDecision::AllowForSession, reason: None },
        Request::LibraryState,
        Request::DetectProviders,
        Request::ListCatalog { provider: ProviderKind::OpenRouter },
        Request::AddModel { provider: ProviderKind::Ollama, model_id: "qwen3:8b".into() },
        Request::RemoveModel { model: mref() },
        Request::SetSlot { slot: Slot::Crew("researcher".into()), model: Some(mref()), effort: Effort::High },
        Request::SetCredential { provider: ProviderKind::OpenRouter, secret: Secret("sk-or-xyz".into()) },
        Request::Status,
        Request::Approve { approval: ApprovalId(4), decision: ApprovalDecision::Deny, reason: Some("use the sandbox copy".into()) },
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
    assert_eq!(r, Request::Ask { session: None, text: "hello".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let r: Request = serde_json::from_str(r#"{"op":"set_slot","slot":"quick"}"#).unwrap();
    assert_eq!(r, Request::SetSlot { slot: Slot::Quick, model: None, effort: Effort::Medium });
    assert!(serde_json::from_str::<Request>(r#"{"op":"set_slot","slot":"chef"}"#).is_err());
}

#[test]
fn approve_without_a_reason_is_the_old_wire_form_and_old_json_still_parses() {
    let plain = Request::Approve { approval: ApprovalId(3), decision: ApprovalDecision::Deny, reason: None };
    let s = serde_json::to_string(&plain).unwrap();
    assert!(!s.contains("reason"), "{s}");
    let old: Request = serde_json::from_str(r#"{"op":"approve","approval":3,"decision":"deny"}"#).unwrap();
    assert_eq!(old, plain);
    let said = Request::Approve { approval: ApprovalId(3), decision: ApprovalDecision::Deny, reason: Some("not that file".into()) };
    assert!(serde_json::to_string(&said).unwrap().contains(r#""reason":"not that file""#));
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
        Event::SessionNotes { session: sid(), notes: vec![StoredNote { at_ms: 4, title: "crew · run r1 done".into(), body: "held".into() }] },
        Event::Transcript {
            session: sid(),
            turns: vec![StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "hi".into() }], at_ms: 5, phases: Vec::new(), numbers: None }],
        },
        Event::TurnStarted { session: sid(), turn: 1, model: mref(), user: "hi".into() },
        Event::TextDelta { session: sid(), turn: 1, text: "Hel".into() },
        Event::ReasoningDelta { session: sid(), turn: 1, text: "hm".into(), tokens: 1 },
        Event::TextRetracted { session: sid(), turn: 1, tokens: 2 },
        Event::ToolStarted { session: sid(), turn: 1, call_id: "c".into(), tool: "read".into(), summary: "src/main.rs".into() },
        Event::ToolFinished { session: sid(), turn: 1, call_id: "c".into(), ok: true, summary: "41 lines".into() },
        Event::TurnFinished { session: sid(), turn: 1, usage: Usage { input: 3, ..Default::default() }, stop: TurnStop::Done },
        Event::ApprovalRequested { approval: ApprovalId(1), session: sid(), tool: "bash".into(), summary: "cargo test".into(), detail: "{}".into(), run: None, proposed_rule: None },
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
        Request::ModelsInstall,
        Request::ModelsDecline,
        Request::MemoryProbe { query: "deploy".into(), workspace: Some("C:/w".into()) },
        Request::MemoryProbe { query: "deploy".into(), workspace: None },
        Request::MemoryProfile { workspace: Some("C:/w".into()) },
        Request::MemoryProfile { workspace: None },
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
    let r: Request = serde_json::from_str(r#"{"op":"memory_profile"}"#).unwrap();
    assert_eq!(r, Request::MemoryProfile { workspace: None });
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
            models: ModelsState::Ready,
        }),
        Event::MemoryStats(MemoryStatsReport { degraded: Some("models missing".into()), ..Default::default() }),
        Event::MemoryStats(MemoryStatsReport { models: ModelsState::Missing { bytes: 193_270_110, cascade: false }, ..Default::default() }),
        Event::MemoryStats(MemoryStatsReport { models: ModelsState::Installing { done: 78, total: 186 }, ..Default::default() }),
        Event::MemoryStats(MemoryStatsReport { models: ModelsState::Failed { reason: "not published".into() }, ..Default::default() }),
        Event::MemoryStats(MemoryStatsReport { models: ModelsState::Declined, ..Default::default() }),
        Event::Recalled { session: sid(), turn: 2, count: 3, summary: "deploy on Fridays".into(), total_ms: 0, stages: Vec::new(), abstained: false },
        Event::ImportProgress { read: 50, imported: 40 },
        Event::ImportDone { report },
        Event::LegacyAvailable { path: "C:/old".into(), approx_entries: 1200, already_imported: false },
        Event::MemoryProbed { recalled: vec![hit()], abstained: None, latency_ms: 41 },
        Event::MemoryProbed { recalled: Vec::new(), abstained: Some("no runner-up".into()), latency_ms: 0 },
        Event::MemoryProfile { hits: vec![hit()], tokens: 22, omitted: 3 },
        Event::MemoryProfile { hits: Vec::new(), tokens: 0, omitted: 0 },
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
    assert_eq!(Event::Recalled { session: sid(), turn: 1, count: 1, summary: String::new(), total_ms: 0, stages: Vec::new(), abstained: false }.session(), Some(&sid()));
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

fn notice(severity: Severity) -> Notice {
    Notice {
        id: "n-1".into(),
        at_ms: 1_700_000_000_000,
        source: NoticeSource::Run,
        severity,
        kind: "finished".into(),
        title: "wnr5 finished".into(),
        detail: "stage wnr-p5 · layer 9".into(),
        subject: "C:/runs/wnr5".into(),
        seen: false,
    }
}

fn watch_state() -> WatchState {
    WatchState {
        roots: vec![WatchRoot {
            path: "C:/runs".into(),
            added_ms: 5,
            runs: 1,
            git: Some(GitInfo {
                branch: "main".into(),
                dirty: 2,
                head: Some("f7f46ce".into()),
                subject: Some("M2.6 profile memory".into()),
                commit_ms: Some(1_700_000_000_000),
                new_commits: 1,
            }),
            problem: None,
        }],
        runs: vec![RunInfo {
            dir: "C:/runs/wnr5".into(),
            name: "marlowe-valiance-22b-wnr5".into(),
            status: RunStatus::Alive,
            stage: Some("wnr-p5".into()),
            layer: Some(9),
            pid: Some(48440),
            heartbeat_ms: Some(1_700_000_000_000),
            last_line: Some("[05:19:34] layer 9 done".into()),
            step: None,
            loss: Some(1.218),
            since_ms: 4,
        }],
        gpu: GpuReport {
            devices: vec![GpuDevice {
                index: 0,
                name: "RTX 4090".into(),
                util_pct: 97,
                mem_used_mb: 22_000,
                mem_total_mb: 24_564,
                temp_c: Some(71),
                processes: vec!["python.exe".into()],
            }],
            unavailable: None,
        },
        ollama: OllamaReport { reachable: true, models: 12, new_models: vec!["marlowe-valiance:22b".into()] },
        quiet: QuietHours::default(),
    }
}

fn candidate() -> WatchCandidate {
    WatchCandidate {
        path: "C:/Users/m/Projects/PruningResearch/valiance".into(),
        reason: "heartbeat.json, 3 logs".into(),
        modified_ms: 9,
        live_runs: 1,
    }
}

#[test]
fn watch_and_voice_requests_round_trip_and_parse_with_defaults() {
    let reqs = vec![
        Request::Brief,
        Request::WatchList,
        Request::WatchAdd { path: "C:/runs".into() },
        Request::WatchRemove { path: "C:/runs".into() },
        Request::WatchCandidates,
        Request::ListNotices { limit: 20, unseen_only: true },
        Request::NoticeSeen { id: "n-1".into() },
        Request::Speak { text: "Good evening, sir.".into() },
        Request::StopSpeaking,
        Request::VoiceSettings { enabled: Some(true), voice: Some("George".into()) },
        Request::VoiceSettings { enabled: None, voice: None },
    ];
    for r in &reqs {
        let s = round_trip(r);
        assert!(s.starts_with(r#"{"op":""#), "{s}");
    }
    assert_eq!(serde_json::to_string(&Request::Brief).unwrap(), r#"{"op":"brief"}"#);
    assert_eq!(serde_json::to_string(&reqs[2]).unwrap(), r#"{"op":"watch_add","path":"C:/runs"}"#);
    let r: Request = serde_json::from_str(r#"{"op":"list_notices","limit":5}"#).unwrap();
    assert_eq!(r, Request::ListNotices { limit: 5, unseen_only: false });
    let r: Request = serde_json::from_str(r#"{"op":"voice_settings"}"#).unwrap();
    assert_eq!(r, Request::VoiceSettings { enabled: None, voice: None });
    let r: Request = serde_json::from_str(r#"{"op":"stop_speaking"}"#).unwrap();
    assert_eq!(r, Request::StopSpeaking);
}

#[test]
fn watch_and_voice_events_round_trip_inside_an_envelope() {
    let events = vec![
        Event::Brief {
            text: "Good morning, sir. wnr5 finished at 04:12.".into(),
            spoken: "Good morning, sir. w n r 5 finished at four twelve.".into(),
            items: vec![BriefItem {
                kind: BriefItemKind::Finished,
                severity: Severity::Good,
                text: "wnr5 finished at 04:12".into(),
                subject: Some("C:/runs/wnr5".into()),
            }],
            offers: vec![candidate()],
            session: Some(sid()),
        },
        Event::Brief { text: "Quiet night, sir.".into(), spoken: "Quiet night, sir.".into(), items: vec![], offers: vec![], session: None },
        Event::Notice(notice(Severity::Alarm)),
        Event::Notices { list: vec![notice(Severity::Info), Notice { seen: true, ..notice(Severity::Attention) }] },
        Event::Notices { list: vec![] },
        Event::WatchState(watch_state()),
        Event::WatchState(WatchState::default()),
        Event::WatchCandidates { candidates: vec![candidate()] },
        Event::VoiceState(VoiceStatus {
            available: true,
            enabled: true,
            voice: Some("George (en-GB)".into()),
            speaking: true,
            envelope: vec![0, 40, 180, 255, 12],
        }),
        Event::VoiceState(VoiceStatus::default()),
    ];
    for (i, ev) in events.into_iter().enumerate() {
        assert_eq!(ev.session(), None, "watch events are not session events");
        let env = Envelope { seq: i as u64, at_ms: 1_700_000_000_000, event: ev };
        let s = round_trip(&env);
        assert!(s.contains(r#""ev":""#), "{s}");
    }
    let s = serde_json::to_string(&Event::Notice(notice(Severity::Alarm))).unwrap();
    assert!(s.contains(r#""ev":"notice""#) && s.contains(r#""severity":"alarm""#) && s.contains(r#""source":"run""#), "{s}");
    // Minimal shapes decode with defaults.
    let ev: Event = serde_json::from_str(r#"{"ev":"brief","text":"t","spoken":"s"}"#).unwrap();
    assert_eq!(ev, Event::Brief { text: "t".into(), spoken: "s".into(), items: vec![], offers: vec![], session: None });
    let ev: Event = serde_json::from_str(
        r#"{"ev":"watch_state","roots":[],"runs":[],"gpu":{"unavailable":"nvml.dll not found"},"ollama":{"reachable":false,"models":0}}"#,
    )
    .unwrap();
    let Event::WatchState(w) = ev else { panic!() };
    assert_eq!(w.gpu.unavailable.as_deref(), Some("nvml.dll not found"));
    assert_eq!(w.quiet, QuietHours::default());
    let ev: Event = serde_json::from_str(r#"{"ev":"voice_state","available":false,"enabled":false,"speaking":false}"#).unwrap();
    assert_eq!(ev, Event::VoiceState(VoiceStatus::default()));
}

#[test]
fn severities_order_and_quiet_hours_wrap_midnight() {
    assert!(Severity::Info < Severity::Good && Severity::Good < Severity::Attention && Severity::Attention < Severity::Alarm);
    assert!(!Severity::Good.is_urgent() && Severity::Attention.is_urgent() && Severity::Alarm.is_urgent());
    let q = QuietHours::default();
    assert_eq!(q.label(), "23:00–07:00");
    assert!(q.contains(23 * 60) && q.contains(0) && q.contains(6 * 60 + 59));
    assert!(!q.contains(7 * 60) && !q.contains(12 * 60) && !q.contains(22 * 60 + 59));
    let day = QuietHours { start_min: 12 * 60, end_min: 13 * 60 };
    assert!(day.contains(12 * 60 + 30) && !day.contains(13 * 60) && !day.contains(0));
    assert!(!QuietHours { start_min: 60, end_min: 60 }.contains(60), "an empty window is never quiet");
    for (s, status) in [("\"alive\"", RunStatus::Alive), ("\"stalled\"", RunStatus::Stalled), ("\"finished\"", RunStatus::Finished)] {
        assert_eq!(serde_json::to_string(&status).unwrap(), s);
    }
    assert!(RunStatus::Stalled.is_live() && !RunStatus::Failed.is_live());
}

#[test]
fn user_request_and_event_round_trip_and_parse_with_defaults() {
    let r = Request::SetUser { name: Some("Matthew".into()), address: Some("sir".into()) };
    assert_eq!(round_trip(&r), r#"{"op":"set_user","name":"Matthew","address":"sir"}"#);
    let r: Request = serde_json::from_str(r#"{"op":"set_user"}"#).unwrap();
    assert_eq!(r, Request::SetUser { name: None, address: None });
    let ev = Event::User { name: Some("Matthew".into()), address: None, set: true };
    let env = Envelope { seq: 0, at_ms: 1, event: ev.clone() };
    assert!(round_trip(&env).contains(r#""ev":"user""#));
    assert_eq!(ev.session(), None);
    let ev: Event = serde_json::from_str(r#"{"ev":"user"}"#).unwrap();
    assert_eq!(ev, Event::User { name: None, address: None, set: false });
}

#[test]
fn address_forms_parse_resolve_and_classify() {
    assert_eq!("sir".parse::<AddressForm>().unwrap(), AddressForm::Sir);
    assert_eq!(" Name ".parse::<AddressForm>().unwrap(), AddressForm::Name);
    assert_eq!("none".parse::<AddressForm>().unwrap(), AddressForm::Nothing);
    assert!("madam".parse::<AddressForm>().is_err());
    assert_eq!(AddressForm::Sir.resolve(Some("Matthew")).as_deref(), Some("sir"));
    assert_eq!(AddressForm::Name.resolve(Some(" Matthew ")).as_deref(), Some("Matthew"));
    assert_eq!(AddressForm::Name.resolve(None), None);
    assert_eq!(AddressForm::Nothing.resolve(Some("Matthew")), None);
    assert_eq!(AddressForm::classify(Some("sir"), None), Some(AddressForm::Sir));
    assert_eq!(AddressForm::classify(Some("Matthew"), Some("Matthew")), Some(AddressForm::Name));
    assert_eq!(AddressForm::classify(None, Some("Matthew")), Some(AddressForm::Nothing));
    assert_eq!(AddressForm::classify(Some("Doctor"), Some("Matthew")), None);
    for f in AddressForm::ALL {
        assert_eq!(f.to_string().parse::<AddressForm>().unwrap(), f);
    }
}

#[test]
fn user_fields_are_trimmed_and_bounded() {
    assert_eq!(clean_user_field("name", Some("  Matthew ")).unwrap().as_deref(), Some("Matthew"));
    assert_eq!(clean_user_field("name", Some("   ")).unwrap(), None);
    assert_eq!(clean_user_field("name", None).unwrap(), None);
    assert!(clean_user_field("name", Some("Matt\nhew")).is_err());
    assert!(clean_user_field("name", Some(&"x".repeat(USER_FIELD_MAX + 1))).is_err());
    assert!(clean_user_field("name", Some(&"é".repeat(USER_FIELD_MAX))).is_ok());
}

// ---- phases (additive wire)

fn span(kind: PhaseKind, start_ms: u64, ms: u64) -> PhaseSpan {
    PhaseSpan { kind, stage: None, start_ms, ms, n: None, detail: None, parent: None, est: false }
}

#[test]
fn phase_events_round_trip_and_stay_small() {
    let evs = vec![
        Event::Phase(PhaseEvent::start(PhaseKind::Recall).on(&sid(), 2).stage("embed")),
        Event::Phase(PhaseEvent::progress(PhaseKind::Recall, 2940, 4198).on(&sid(), 2).stage("dense").unit("items")),
        Event::Phase(PhaseEvent::end(PhaseKind::Recall, 12).on(&sid(), 2).stage("dense")),
        Event::Phase(PhaseEvent { est: true, rate: Some(1900.0), ..PhaseEvent::progress(PhaseKind::Prefill, 5000, 8200) }),
        Event::TurnTimings {
            session: sid(),
            turn: 2,
            spans: vec![span(PhaseKind::Sending, 0, 1), PhaseSpan { stage: Some("embed".into()), n: Some(4), parent: Some(PhaseKind::Recall), ..span(PhaseKind::Recall, 5, 12) }],
            numbers: TurnNumbers { ttft_ms: Some(80), enter_to_wire_ms: Some(6), token_to_bus_ms: Some(0), read: Some(ReadStat { new_tok: 8200, cached_tok: 0, ms: 1900 }) },
        },
        Event::Recalled {
            session: sid(),
            turn: 2,
            count: 0,
            summary: String::new(),
            total_ms: 84,
            stages: vec![StageMs { name: "embed".into(), ms: 12, n: None }, StageMs { name: "dense".into(), ms: 4, n: Some(4198) }],
            abstained: true,
        },
    ];
    for ev in evs {
        let s = round_trip(&Envelope { seq: 7, at_ms: 1, event: ev });
        assert!(!s.contains("null"), "absent fields are omitted, not null: {s}");
    }
    let s = serde_json::to_string(&Event::Phase(PhaseEvent::start(PhaseKind::Admit))).unwrap();
    assert_eq!(s, r#"{"ev":"phase","turn":0,"kind":"admit","state":"start"}"#);
    let t = Event::TurnTimings { session: sid(), turn: 1, spans: Vec::new(), numbers: TurnNumbers::default() };
    assert_eq!(Some(&sid()), t.session());
    assert_eq!(Some(&sid()), Event::Phase(PhaseEvent::start(PhaseKind::Recall).on(&sid(), 0)).session());
}

#[test]
fn old_wire_shapes_still_decode_and_new_ones_are_skippable() {
    // Recalled from before stages existed.
    let ev: Event = serde_json::from_str(r#"{"ev":"recalled","session":"s1","turn":1,"count":2,"summary":"x"}"#).unwrap();
    assert_eq!(
        ev,
        Event::Recalled { session: sid(), turn: 1, count: 2, summary: "x".into(), total_ms: 0, stages: Vec::new(), abstained: false }
    );
    // A stored turn and an Ask from before phases and client stamps.
    let t: StoredTurn = serde_json::from_str(r#"{"idx":0,"role":"user","content":[],"at_ms":5}"#).unwrap();
    assert!(t.phases.is_empty() && t.numbers.is_none());
    let a: Request = serde_json::from_str(r#"{"op":"ask","text":"hi"}"#).unwrap();
    assert!(matches!(a, Request::Ask { client_ms: None, .. }));
    // A phase kind a newer daemon invents is Unknown, not an error.
    let ev: Event = serde_json::from_str(r#"{"ev":"phase","kind":"warp_drive","state":"start"}"#).unwrap();
    assert!(matches!(ev, Event::Phase(PhaseEvent { kind: PhaseKind::Unknown, .. })));
    // An event this reader has never heard of fails to parse, which every client's line reader
    // skips; that is what lets new events ship without a protocol bump.
    assert!(serde_json::from_str::<Envelope>(r#"{"seq":1,"at_ms":1,"event":{"ev":"from_the_future"}}"#).is_err());
    assert_eq!(PROTO_VERSION, 1);
}
