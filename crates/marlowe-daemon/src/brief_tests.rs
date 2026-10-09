//! Brief composition against a fixed clock and synthetic runs, notices and results; the polish
//! path against a scripted provider. No daemon, no network, no inference.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use chrono::TimeZone;
use marlowe_model::{
    fold_events, Completion, ModelInfo, ProviderError, ProviderKind, StopReason, StreamEvent, SystemBlock,
};
use marlowe_proto::{BriefItemKind, GitInfo, MemoryStatsReport, Notice, NoticeSource, RunInfo, RunStatus, Severity, WatchCandidate};

use super::*;

const H: u64 = 3600;
const ROOT: &str = r"C:\Users\m\Projects\PruningResearch\valiance";
const RUNS: &str = r"C:\Users\m\Projects\PruningResearch\valiance\runs";

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().expect("an unambiguous local time")
}

fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    at(y, mo, d, h, mi).timestamp_millis()
}

fn inputs(now: DateTime<Local>, away_h: Option<u64>) -> BriefInputs {
    BriefInputs {
        now,
        away: away_h.map(|h| Duration::from_secs(h * H)),
        first_today: true,
        address: Some("sir".into()),
        notices: Vec::new(),
        runs: Vec::new(),
        new_models: Vec::new(),
        git: Vec::new(),
        memory: None,
        memory_news: MemoryNews::default(),
        roots: Vec::new(),
        results: Vec::new(),
        offers: Vec::new(),
        sentinel: true,
    }
}

fn notice(source: NoticeSource, severity: Severity, kind: &str, title: &str, detail: &str, subject: &str, at_ms: i64) -> Notice {
    Notice {
        id: format!("{kind}:{subject}"),
        at_ms,
        source,
        severity,
        kind: kind.into(),
        title: title.into(),
        detail: detail.into(),
        subject: subject.into(),
        seen: false,
    }
}

fn run(dir: &str, name: &str, status: RunStatus, since_ms: i64) -> RunInfo {
    RunInfo {
        dir: dir.into(),
        name: name.into(),
        status,
        stage: None,
        layer: None,
        pid: None,
        heartbeat_ms: None,
        last_line: None,
        step: None,
        loss: None,
        since_ms,
    }
}

fn ended(dir: &str, name: &str, since_ms: i64, last: &str) -> RunInfo {
    RunInfo { last_line: Some(last.into()), ..run(dir, name, RunStatus::Ended, since_ms) }
}

fn memory(beliefs: u64, episodes: u64) -> MemoryStatsReport {
    MemoryStatsReport { beliefs, episodes, ..Default::default() }
}

fn sentences(text: &str) -> usize {
    sentence_count(text)
}

fn offer(path: &str, live_runs: u32) -> WatchCandidate {
    WatchCandidate { path: path.into(), reason: "heartbeat.json, 3 logs".into(), modified_ms: 0, live_runs }
}

fn root(pipelines: u32) -> BriefRoot {
    BriefRoot { path: ROOT.into(), pipelines }
}

/// `{suite}.{metric}` for four suites, from `(loss, kl, top1, retained)` rows.
fn suites(rows: [(f64, f64, f64, f64); 4]) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for (suite, (loss, kl, top1, retained)) in ["english", "math", "code", "reasoning"].into_iter().zip(rows) {
        for (m, v) in [("loss", loss), ("kl", kl), ("top1", top1), ("retained", retained)] {
            out.push((format!("{suite}.{m}"), v));
        }
    }
    out
}

fn audit(now: Vec<(String, f64)>, before: Vec<(String, f64)>) -> ResultComparison {
    ResultComparison {
        path: format!(r"{ROOT}\scratch\27b\audit2_marlowe-valiance-22b-final.json"),
        stem: "audit2_marlowe-valiance-22b-final".into(),
        sibling: "audit2_marlowe-valiance-22b-wnr7".into(),
        sibling_modified_ms: ms(2026, 10, 9, 6, 32),
        modified_ms: ms(2026, 10, 9, 6, 34),
        now,
        before,
    }
}

/// wnr7 -> final: every suite's KL improves, code's most.
fn wnr7() -> Vec<(String, f64)> {
    suites([(2.3502, 0.1301, 0.8350, 0.9790), (0.7900, 0.0290, 0.9530, 0.9950), (0.8300, 0.0843, 0.9050, 0.9870), (1.0600, 0.1550, 0.8990, 0.9930)])
}

fn final_() -> Vec<(String, f64)> {
    suites([(2.3321, 0.1220, 0.8391, 0.9818), (0.7809, 0.0280, 0.9547, 0.9968), (0.8166, 0.0710, 0.9122, 0.9912), (1.0455, 0.1485, 0.9019, 0.9947)])
}

/// The live night1 morning: a heartbeat run that shipped, two log directories that ended
/// yesterday with noisy last lines, an audit family wnr7 -> final, and a discovery candidate.
fn valiance_morning() -> BriefInputs {
    let now = at(2026, 10, 9, 8, 14);
    let mut inp = inputs(now, Some(9));
    let mut shipped = ended(
        &format!(r"{RUNS}\night1"),
        "marlowe-valiance-22b-wnr-final",
        ms(2026, 10, 9, 6, 54),
        "ship: marlowe-valiance-22b-wnr-final-truewidth-q4_k_s.gguf written, check OK 06:54",
    );
    shipped.heartbeat_ms = Some(ms(2026, 10, 9, 6, 39));
    shipped.stage = Some("audit".into());
    shipped.layer = Some(63);
    inp.runs = vec![
        shipped,
        ended(&format!(r"{ROOT}\coherence"), "coherence", ms(2026, 10, 8, 8, 22), "llama_model_quantize_impl: quant size  = 14853.78 MiB (4.63 BPW)"),
        ended(
            &format!(r"{ROOT}\longhorizon"),
            "longhorizon",
            ms(2026, 10, 8, 10, 29),
            "126.47.966.376 I slot print_timing: id  0 | task 62617 | n_gen =   2686, tg =   8.93 t/s, tg_3s =   8.46 t/s",
        ),
    ];
    inp.roots = vec![root(1)];
    inp.results = vec![audit(final_(), wnr7())];
    inp.offers = vec![offer(r"C:\Users\m\Projects\PruningResearch\engine", 0)];
    inp.memory = Some(memory(0, 0));
    inp
}

// ---------------------------------------------------------------- golden briefs

#[test]
fn golden_a_shipped_pipeline_its_audit_and_yesterdays_runs() {
    let b = compose(&valiance_morning());
    assert_eq!(
        b.text,
        r"Good morning, sir. Valiance finished at 06:54 — the wnr-final truewidth Q4_K_S GGUF is written and its check passed. Its audit beat wnr7 on all four suites; code KL is down to 0.071 with 99.1 percent retained. Coherence finished and longhorizon stopped yesterday. PruningResearch\engine has fresh runs — shall I watch it?"
    );
    assert_eq!(
        b.spoken,
        "Good morning, sir. Valiance finished at 06:54, the wnr-final truewidth Q4 K S GGUF is written and its check passed. \
         Its audit beat wnr7 on all four suites, code KL is down to 0.071 with 99.1 percent retained. Coherence finished and \
         longhorizon stopped yesterday. The engine folder has fresh runs, shall I watch it?"
    );
    assert!(word_count(&b.text) <= MAX_WORDS && sentences(&b.text) <= MAX_SENTENCES, "{}", b.text);
    let kinds: Vec<BriefItemKind> = b.items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [BriefItemKind::Finished; 4], "the lead, the audit, coherence, longhorizon; no memory padding");
    assert_eq!(b.items[0].text, "Valiance finished at 06:54 — the wnr-final truewidth Q4_K_S GGUF is written and its check passed");
    assert_eq!(b.items[2].text, "longhorizon stopped yesterday at 10:29");
    assert_eq!(b.items[3].text, "coherence finished yesterday at 08:22");
    for raw in ["llama_model", "print_timing", "MiB", "|", "Memory", "In the last day", "ended"] {
        assert!(!b.text.contains(raw), "{raw:?} leaked: {}", b.text);
    }
    assert!(!b.is_quiet());
}

#[test]
fn golden_a_failure_says_why_and_never_quotes_the_line() {
    let now = at(2026, 10, 9, 8, 14);
    let mut inp = inputs(now, Some(9));
    let mut oom = run(&format!(r"{RUNS}\night1"), "marlowe-valiance-22b-wnr-final", RunStatus::Failed, ms(2026, 10, 9, 3, 12));
    oom.heartbeat_ms = Some(ms(2026, 10, 9, 3, 10));
    oom.last_line = Some("torch.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.00 GiB".into());
    inp.runs = vec![oom, ended(&format!(r"{ROOT}\coherence"), "coherence", ms(2026, 10, 8, 23, 40), "convert_hf_to_gguf.py exited with exit code 1")];
    inp.roots = vec![root(1)];
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Valiance failed at 03:12 — out of memory. Coherence failed yesterday (exit code 1).");
    assert_eq!(b.spoken, "Good morning, sir. Valiance failed at 03:12, out of memory. Coherence failed yesterday, exit code 1.");
    assert!(b.items.iter().all(|i| i.kind == BriefItemKind::Attention && i.severity == Severity::Attention), "{:?}", b.items);
}

#[test]
fn golden_a_failed_check_is_never_called_finished() {
    let now = at(2026, 10, 9, 8, 14);
    let mut inp = inputs(now, Some(9));
    let mut shipped = ended(
        &format!(r"{RUNS}\night1"),
        "marlowe-valiance-22b-wnr-final",
        ms(2026, 10, 9, 6, 54),
        "ship: marlowe-valiance-22b-wnr-final-truewidth-q4_k_s.gguf written, check FAILED 06:54",
    );
    shipped.status = RunStatus::Finished;
    shipped.heartbeat_ms = Some(ms(2026, 10, 9, 6, 50));
    inp.runs = vec![shipped];
    inp.roots = vec![root(1)];
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Valiance failed at 06:54 — its check failed.");
    assert!(!b.text.contains("written"), "{}", b.text);
}

#[test]
fn golden_a_quiet_night_is_one_line() {
    let mut inp = inputs(at(2026, 10, 9, 7, 30), Some(10));
    inp.memory = Some(memory(390, 12));
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Nothing moved while you were away.");
    assert_eq!(b.spoken, b.text);
    assert!(b.is_quiet());
    assert!(b.items.is_empty(), "unchanged memory is not news: {:?}", b.items);
    // A first-ever session has no "away".
    let first = compose(&inputs(at(2026, 10, 9, 7, 30), None));
    assert_eq!(first.text, "Good morning, sir. Nothing to report yet.");
}

#[test]
fn memory_is_said_only_when_it_changed() {
    let mut inp = inputs(at(2026, 10, 9, 7, 30), Some(10));
    inp.memory_news = MemoryNews { from_user: 3, other: 0 };
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Nothing moved while you were away. I recorded three new things you told me.");
    assert!(b.is_quiet(), "a memory line is not a run moving");
    inp.memory_news = MemoryNews { from_user: 1, other: 0 };
    assert!(compose(&inp).text.ends_with("I recorded one new thing you told me."));
    inp.memory_news = MemoryNews { from_user: 0, other: 2 };
    assert!(compose(&inp).text.ends_with("I noted two new things about your work."));
    inp.memory_news = MemoryNews { from_user: 2, other: 3 };
    assert!(compose(&inp).text.ends_with("I recorded five new things, two of them from you."));
    inp.memory_news = MemoryNews::default();
    inp.memory = Some(MemoryStatsReport { degraded: Some("embedder missing".into()), ..memory(0, 0) });
    assert!(compose(&inp).text.ends_with("Memory recall is degraded."));
}

// ---------------------------------------------------------------- naming, outcomes, days

#[test]
fn runs_are_named_the_way_the_person_names_them() {
    let night1 = run(&format!(r"{RUNS}\night1"), "marlowe-valiance-22b-wnr-final", RunStatus::Finished, 0);
    assert_eq!(display_name(&night1, &[root(1)]), "Valiance", "one pipeline: the root's name");
    assert_eq!(display_name(&night1, &[root(2)]), "wnr-final", "several: the run without the project prefix");
    assert_eq!(display_name(&night1, &[]), "marlowe-valiance-22b-wnr-final");
    let logs = run(&format!(r"{ROOT}\coherence"), "coherence", RunStatus::Ended, 0);
    assert_eq!(display_name(&logs, &[root(1)]), "coherence", "a log directory keeps its name");
    assert_eq!(words::shorten("marlowe-valiance-265b-c24", "valiance"), "c24");
    assert_eq!(words::shorten("qwen-ft-lr3e4", "valiance"), "qwen-ft-lr3e4");
    assert_eq!(words::shorten("x_valiance_7b", "valiance"), "x_valiance_7b", "nothing left after the prefix");
    assert_eq!(
        words::artifact_phrase("marlowe-valiance-22b-wnr-final-truewidth-q4_k_s.gguf", "marlowe-valiance-22b-wnr-final", "wnr-final", Some("valiance")),
        "wnr-final truewidth Q4_K_S GGUF"
    );
    assert_eq!(words::artifact_phrase("model-final-bf16.safetensors", "other", "other", None), "model final BF16 safetensors");
}

#[test]
fn a_last_line_is_paraphrased_or_left_out() {
    let p = |l: &str| words::paraphrase(l, "marlowe-valiance-22b-wnr-final", "wnr-final", Some("valiance"));
    assert_eq!(p("llama_model_quantize_impl: quant size  = 14853.78 MiB (4.63 BPW)").as_deref(), Some("quantised to 4.63 BPW"));
    assert_eq!(p("[07:01:02] saved out/marlowe-valiance-22b-wnr-final.pt").as_deref(), Some("the wnr-final checkpoint is saved"));
    assert_eq!(p("[06:39:14] === stage audit finished"), None, "a completion with nothing to repeat");
    for noisy in [
        "llama_model_quantize_impl: quant size  = 14853.78 MiB (4.63 BPW)",
        "126.47.966.376 I slot print_timing: id  0 | task 62617",
        "[06:39:14] audit marlowe-valiance-22b-wnr-final: retained english 97.851% math 100.058% code 96.966% reasoning 100.035%",
        "=========================================",
    ] {
        assert!(words::is_noisy(noisy), "{noisy}");
    }
    for clean in ["stage pickfinal finished", "step 1200", "no heartbeat for 42 min", "wrote wnr_27b.log"] {
        assert!(!words::is_noisy(clean), "{clean}");
    }
    // A run the Sentinel could only call ended is read from its last line.
    let o = |status, last: &str| words::outcome(&RunInfo { last_line: Some(last.into()), ..run("d", "d", status, 0) });
    assert_eq!(o(RunStatus::Ended, "ship: x.gguf written, check OK 06:54"), words::Outcome::Finished);
    assert_eq!(o(RunStatus::Ended, "RuntimeError: shape mismatch"), words::Outcome::Failed(Some("RuntimeError".into())));
    assert_eq!(o(RunStatus::Ended, "serving on port 8080"), words::Outcome::Stopped);
    assert_eq!(o(RunStatus::Failed, "ALERT orchestrator is dead"), words::Outcome::Failed(None), "no reason beats 'an error'");
    // The paraphraser never turns a failure or a negation into a success.
    assert_eq!(o(RunStatus::Finished, "ship: x.gguf written, check FAILED"), words::Outcome::Failed(Some("its check failed".into())));
    assert_eq!(o(RunStatus::Ended, "Not written: x.gguf"), words::Outcome::Stopped);
    assert_eq!(o(RunStatus::Ended, "Error writing x.gguf"), words::Outcome::Failed(None));
    assert_eq!(o(RunStatus::Ended, "done, 3 tests failed"), words::Outcome::Failed(Some("its check failed".into())));
    for bad in ["ship: x.gguf written, check FAILED", "ship: x.gguf written, check failed", "Not written: x.gguf", "x.gguf wasn't saved", "Error writing x.gguf"] {
        assert_eq!(p(bad), None, "{bad}");
    }
}

#[test]
fn day_words_follow_the_calendar_not_the_clock() {
    let now = at(2026, 10, 9, 8, 14);
    assert_eq!(words::when(ms(2026, 10, 9, 6, 54), &now), "at 06:54");
    assert_eq!(words::when(ms(2026, 10, 8, 10, 29), &now), "yesterday at 10:29");
    assert_eq!(words::when(ms(2026, 10, 8, 23, 59), &now), "yesterday at 23:59", "nine hours ago is still yesterday");
    assert_eq!(words::when(ms(2026, 10, 6, 14, 0), &now), "on Tuesday");
    assert_eq!(words::when(ms(2026, 10, 1, 14, 0), &now), "on 1 October");
    assert_eq!(words::day_word(ms(2026, 10, 9, 1, 0), &now), None);
    assert_eq!(words::day_word(ms(2026, 10, 8, 1, 0), &now).as_deref(), Some("yesterday"));
}

#[test]
fn runs_after_the_lead_are_grouped_by_how_and_when() {
    let now = at(2026, 10, 9, 8, 14);
    let o = |n: &str, out: words::Outcome, t: i64| (n.to_string(), out, t);
    let y = ms(2026, 10, 8, 9, 0);
    let fin = words::Outcome::Finished;
    assert_eq!(others_text(&[o("a", fin.clone(), y), o("b", fin.clone(), y)], &now), "a and b finished yesterday");
    assert_eq!(
        others_text(&[o("a", fin.clone(), ms(2026, 10, 9, 5, 10)), o("b", words::Outcome::Stopped, ms(2026, 10, 9, 6, 0))], &now),
        "a finished at 05:10 and b stopped at 06:00"
    );
    assert_eq!(
        others_text(&[o("a", fin.clone(), y), o("b", words::Outcome::Failed(Some("out of memory".into())), y)], &now),
        "a finished and b (out of memory) failed yesterday"
    );
    assert_eq!(
        others_text(&[o("a", fin, ms(2026, 10, 6, 9, 0)), o("b", words::Outcome::Stopped, y)], &now),
        "a finished on Tuesday and b stopped yesterday"
    );
}

// ---------------------------------------------------------------- results

#[test]
fn a_result_names_its_best_and_worst_movement_when_it_did_not_win_everywhere() {
    let mut now_ = final_();
    // math KL got worse; code is still the best movement.
    for (m, v) in now_.iter_mut() {
        if m == "math.kl" {
            *v = 0.0321;
        }
    }
    let (t, beat) = words::result_sentence(&audit(now_, wnr7()), "Its").unwrap();
    assert_eq!(t, "Its audit beat wnr7 on three of four suites; code KL is down to 0.071, but math KL is up to 0.0321");
    assert!(!beat);
    // Nothing improved: the worst movement only.
    let (t, _) = words::result_sentence(&audit(wnr7(), final_()), "The latest").unwrap();
    assert_eq!(t, "The latest audit trailed wnr7 on all four suites; code KL is up to 0.0843");
    // A flat file is one suite: both numbers of its primary metric.
    let flat = |kl: f64| vec![("kl".to_string(), kl), ("unigram".to_string(), 7.2)];
    let r = ResultComparison { stem: "eval_ft".into(), sibling: "eval_base".into(), now: flat(0.0803), before: flat(0.0912), ..audit(vec![], vec![]) };
    assert_eq!(words::result_sentence(&r, "Its").unwrap().0, "Its eval beat base: KL is down to 0.0803, from 0.0912");
    // Shares are said in percent; names the heuristics do not know are never judged.
    let acc = |a: f64| vec![("thinking.top1".to_string(), a), ("thinking.unigram".to_string(), 7.0)];
    let r = ResultComparison { now: acc(0.8849), before: acc(0.8823), ..r.clone() };
    assert_eq!(words::result_sentence(&r, "Its").unwrap().0, "Its eval beat base: thinking top-1 is up to 88.5 percent, from 88.2 percent");
    let unknown = ResultComparison { now: vec![("unigram".into(), 7.0)], before: vec![("unigram".into(), 7.2)], ..r };
    assert_eq!(words::result_sentence(&unknown, "Its"), None);
}

#[test]
fn a_newer_result_with_only_a_stale_sibling_gives_way_to_a_fresh_pair() {
    // The live root: audit_..-wnr-final (06:39) matches nothing newer than a two-day-old audit_.
    let mut inp = valiance_morning();
    let stale = ResultComparison {
        path: format!(r"{ROOT}\scratch\27b\audit_marlowe-valiance-22b-wnr-final.json"),
        stem: "audit_marlowe-valiance-22b-wnr-final".into(),
        sibling: "audit_marlowe-valiance-22b-h0c512".into(),
        sibling_modified_ms: ms(2026, 10, 7, 6, 44),
        modified_ms: ms(2026, 10, 9, 6, 39),
        now: final_(),
        before: wnr7(),
    };
    inp.results.insert(0, stale.clone());
    assert!(compose(&inp).text.contains("Its audit beat wnr7 on all four suites;"), "{}", compose(&inp).text);
    // Alone, it is still said: a stale comparison beats none.
    inp.results = vec![stale];
    assert!(compose(&inp).text.contains("Its audit beat h0c512 on all four suites;"), "{}", compose(&inp).text);
}

#[test]
fn a_result_outside_the_lead_is_the_latest_not_its() {
    let mut inp = valiance_morning();
    inp.results[0].path = r"D:\elsewhere\audit2_x-final.json".into();
    let b = compose(&inp);
    assert!(b.text.contains(" The latest audit beat wnr7 on all four suites;"), "{}", b.text);
}

// ---------------------------------------------------------------- live runs and attention

#[test]
fn a_morning_with_two_pipelines_names_each_by_its_short_name() {
    let now = at(2026, 10, 9, 8, 14);
    let dir5 = format!(r"{RUNS}\wnr5");
    let dir6 = format!(r"{RUNS}\wnr6");
    let mut inp = inputs(now, Some(9));
    let finished_at = ms(2026, 10, 9, 4, 12);
    inp.notices = vec![
        notice(NoticeSource::Run, Severity::Attention, "stalled", "marlowe-valiance-22b-wnr6 stalled at layer 9", "no heartbeat for 42 min", &dir6, finished_at + 1000),
        notice(NoticeSource::Run, Severity::Good, "finished", "marlowe-valiance-22b-wnr5 finished", "stage wnr-p5 · layer 9", &dir5, finished_at),
    ];
    let mut stalled = run(&dir6, "marlowe-valiance-22b-wnr6", RunStatus::Stalled, finished_at);
    stalled.layer = Some(9);
    stalled.heartbeat_ms = Some(now.timestamp_millis() - 42 * 60_000);
    let mut done = run(&dir5, "marlowe-valiance-22b-wnr5", RunStatus::Finished, finished_at);
    done.heartbeat_ms = Some(finished_at);
    inp.runs = vec![done, stalled];
    inp.roots = vec![root(2)];
    inp.memory = Some(memory(390, 12));

    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. wnr5 finished at 04:12. One thing needs you: wnr6 has stalled at layer 9 (no heartbeat for 42 min).");
    let kinds: Vec<BriefItemKind> = b.items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [BriefItemKind::Finished, BriefItemKind::Attention], "each run once: its state covers its notices");
}

#[test]
fn live_runs_are_listed_last_and_a_stall_needs_you() {
    let now = at(2026, 10, 9, 14, 0);
    let mut inp = inputs(now, Some(3));
    let mut alive = run(&format!(r"{RUNS}\wnr7"), "wnr7", RunStatus::Alive, 0);
    alive.stage = Some("wnr-p2".into());
    alive.layer = Some(3);
    let mut stalled = run(&format!(r"{RUNS}\wnr6"), "wnr6", RunStatus::Stalled, 0);
    stalled.layer = Some(9);
    stalled.heartbeat_ms = Some(now.timestamp_millis() - (2 * 60 + 5) * 60_000);
    let mut failed = run(&format!(r"{RUNS}\wnr8"), "wnr8", RunStatus::Failed, ms(2026, 10, 9, 12, 30));
    failed.last_line = Some("torch.cuda.OutOfMemoryError: CUDA out of memory".into());
    inp.runs = vec![alive, stalled, failed];
    let b = compose(&inp);
    assert_eq!(
        b.text,
        "Good afternoon, sir. wnr8 failed at 12:30 — out of memory. One thing needs you: wnr6 has stalled at layer 9 \
         (no heartbeat for 2 h 5 min). wnr7 (stage wnr-p2, layer 3) is still running."
    );
}

#[test]
fn a_stage_ending_inside_a_live_run_is_not_the_run_finishing() {
    // Shape of the live night1 case: stage_27b.log and wnr_27b.log both end a stage.
    let now = at(2026, 10, 9, 6, 40);
    let dir = format!(r"{RUNS}\night1");
    let mut inp = inputs(now, Some(9));
    let t = now.timestamp_millis() - 240_000;
    for log in ["wnr_27b.log", "stage_27b.log"] {
        inp.notices.push(notice(
            NoticeSource::Log,
            Severity::Good,
            "completion",
            &format!("{log}: completed at 06:35:48"),
            "[06:35:48] === stage prune finished",
            &format!(r"{dir}\{log}"),
            t,
        ));
    }
    let mut live = run(&dir, "marlowe-valiance-22b-wnr-final", RunStatus::Alive, 0);
    live.heartbeat_ms = Some(t);
    live.stage = Some("audit".into());
    live.layer = Some(32);
    inp.runs = vec![live];
    inp.roots = vec![root(1)];
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Valiance (stage audit, layer 32; stage prune finished at 06:35) is still running.");
    assert!(b.items.iter().all(|i| i.kind == BriefItemKind::Running));
}

#[test]
fn a_stall_the_run_has_moved_on_from_is_not_something_needing_you() {
    let now = at(2026, 10, 9, 8, 14);
    let dir6 = format!(r"{RUNS}\wnr6");
    let t = ms(2026, 10, 9, 2, 0);
    for status in [RunStatus::Finished, RunStatus::Alive] {
        let mut inp = inputs(now, Some(9));
        inp.notices = vec![notice(NoticeSource::Run, Severity::Attention, "stalled", "wnr6 stalled", "no heartbeat for 42 min", &dir6, t)];
        inp.runs = vec![run(&dir6, "wnr6", status, ms(2026, 10, 9, 4, 12))];
        let b = compose(&inp);
        assert!(b.items.iter().all(|i| i.kind != BriefItemKind::Attention), "{status:?}: {}", b.text);
        assert!(!b.text.contains("stalled"), "{}", b.text);
    }
    let mut inp = inputs(now, Some(9));
    inp.notices = vec![notice(NoticeSource::Run, Severity::Attention, "stalled", "wnr6 stalled", "no heartbeat for 42 min", &dir6, t)];
    inp.runs = vec![run(&dir6, "wnr6", RunStatus::Stalled, t)];
    assert!(compose(&inp).text.contains("One thing needs you: wnr6 has stalled"));
}

#[test]
fn a_busy_brief_keeps_to_five_sentences_by_priority() {
    let now = at(2026, 10, 9, 8, 0);
    let mut inp = inputs(now, Some(9));
    let t = now.timestamp_millis();
    inp.notices = vec![
        notice(NoticeSource::Trainer, Severity::Good, "finished", "ft-a finished training", "step 900 · loss 1.82", r"C:\r\a", t),
        notice(NoticeSource::Log, Severity::Alarm, "nan", "NaN loss in ft-b", "step 1200", r"C:\r\b\train.log", t),
        notice(NoticeSource::Ollama, Severity::Info, "new_model", "new model marlowe-x", "", "marlowe-x:latest", t),
        notice(NoticeSource::Gpu, Severity::Info, "hot", "GPU at 83 C", "", "gpu0", t),
    ];
    inp.runs = vec![run(r"C:\r\c", "ft-c", RunStatus::Alive, 0), run(r"C:\r\d", "ft-d", RunStatus::Alive, 0)];
    inp.new_models = vec!["marlowe-x:latest".into(), "qwen3:8b".into()];
    inp.git = vec![(r"C:\Users\m\Projects\MarloweCowork".into(), GitInfo { branch: "main".into(), dirty: 3, new_commits: 2, ..Default::default() })];
    inp.memory = Some(memory(4212, 318));
    inp.offers = vec![offer(r"C:\Users\m\Projects\PruningResearch\valiance", 2)];

    let b = compose(&inp);
    assert_eq!(
        b.text,
        r"Good morning, sir. ft-a finished at 08:00. One thing needs you: NaN loss in ft-b (step 1200). Still running: ft-c and ft-d. PruningResearch\valiance has live runs — shall I watch it?"
    );
    assert_eq!(b.offers.len(), 1);
    assert!(sentences(&b.text) <= MAX_SENTENCES);
    let count = |k| b.items.iter().filter(|i| i.kind == k).count();
    assert_eq!(count(BriefItemKind::Model), 2, "the notice and the state name marlowe-x once");
    assert_eq!(count(BriefItemKind::Git), 1);
    assert_eq!(count(BriefItemKind::Memory), 0);
    assert!(b.items.iter().all(|i| !i.text.contains("GPU")), "GPU chatter stays on the board");
    assert_eq!(b.items.iter().find(|i| i.kind == BriefItemKind::Git).unwrap().text, "MarloweCowork: 2 new commits on main, 3 uncommitted");

    // With fewer sections, models and git fit, in canonical order before the offer.
    inp.notices.truncate(1);
    inp.runs.clear();
    assert_eq!(
        compose(&inp).text,
        r"Good morning, sir. ft-a finished at 08:00. New in Ollama: marlowe-x:latest and qwen3:8b. MarloweCowork: 2 new commits on main, 3 uncommitted. PruningResearch\valiance has live runs — shall I watch it?"
    );
}

#[test]
fn a_long_brief_keeps_to_ninety_words() {
    let now = at(2026, 10, 9, 8, 0);
    let mut inp = valiance_morning();
    inp.now = now;
    let long = "the orchestrator for the twenty seven billion parameter refit lost its worker pool and is waiting on a lock";
    inp.notices = (0..3)
        .map(|i| notice(NoticeSource::Log, Severity::Alarm, "error", &format!("{long} {i}"), "", &format!(r"C:\r\x{i}\train.log"), 0))
        .collect();
    let b = compose(&inp);
    assert!(word_count(&b.text) <= MAX_WORDS, "{} words: {}", word_count(&b.text), b.text);
    assert!(sentences(&b.text) <= MAX_SENTENCES);
    assert!(b.text.starts_with("Good morning, sir. Valiance finished at 06:54"), "the lead survives: {}", b.text);
}

/// The brief over a real watch root, through a real Sentinel (read-only on the root; its state
/// lives in a temp home). Prints the brief; asserts only its shape.
/// `MARLOWE_BRIEF_ROOT=<dir> cargo test -p marlowe-daemon --lib real_root -- --ignored --nocapture`
#[test]
#[ignore]
fn real_root_smoke() {
    let Ok(root) = std::env::var("MARLOWE_BRIEF_ROOT") else { return };
    let home = std::env::temp_dir().join(format!("brief-real-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let (tx, _rx) = crossbeam_channel::unbounded();
    let cfg = marlowe_sentinel::SentinelConfig { gpu: false, ollama_addr: None, ..Default::default() };
    let s = marlowe_sentinel::Sentinel::start_with(&home, vec![root.clone().into()], tx, cfg).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut state = s.state();
    while state.runs.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        state = s.state();
    }
    let now = Local::now();
    let since = now.timestamp_millis() - 36 * 3_600_000;
    let mut inp = inputs(now, Some(9));
    inp.roots = state
        .roots
        .iter()
        .map(|r| BriefRoot { path: r.path.clone(), pipelines: state.runs.iter().filter(|x| within(&x.dir, &r.path) && is_pipeline(x)).count() as u32 })
        .collect();
    inp.runs = state.runs.into_iter().filter(|r| r.status.is_live() || r.since_ms >= since).collect();
    // The result files a day of metric notices would name, newest first.
    let mut files: Vec<(std::path::PathBuf, i64)> = Vec::new();
    let mut stack = vec![(std::path::PathBuf::from(&root), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (p, meta) = (e.path(), e.metadata().unwrap());
            if meta.is_dir() && depth < 4 {
                stack.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x == "json") {
                let t = DateTime::<Local>::from(meta.modified().unwrap()).timestamp_millis();
                // Only files with a metric of known direction raise a result notice.
                let judged = marlowe_sentinel::metrics::read_flat(&p, 1 << 20)
                    .is_some_and(|m| m.iter().any(|(k, _)| marlowe_sentinel::metrics::direction(k).is_some()));
                if t >= since && judged {
                    files.push((p, t));
                }
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    inp.results = files.iter().take(3).filter_map(|(p, _)| s.result_comparison(p)).collect();
    let b = compose(&inp);
    s.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    println!("{}\n\n{}\n\n{:#?}", b.text, b.spoken, b.items);
    assert!(word_count(&b.text) <= MAX_WORDS && sentences(&b.text) <= MAX_SENTENCES);
}

// ---------------------------------------------------------------- greeting, absence, offers

#[test]
fn late_night_greets_dryly_and_day_parts_split_on_the_hour() {
    let b = compose(&inputs(at(2026, 10, 10, 1, 40), Some(3)));
    assert_eq!(b.text, "Up late, sir. Nothing moved while you were away.");
    assert_eq!(greeting(DayPart::LateNight, None), "Up late.");
    assert_eq!(greeting(DayPart::Evening, Some("  ")), "Good evening.");
    assert_eq!(greeting(DayPart::Afternoon, Some("ma'am")), "Good afternoon, ma'am.");
    let part = |h| DayPart::of(&at(2026, 10, 9, h, 0));
    assert_eq!(
        [part(4), part(5), part(11), part(12), part(17), part(18), part(22), part(23)],
        [
            DayPart::LateNight,
            DayPart::Morning,
            DayPart::Morning,
            DayPart::Afternoon,
            DayPart::Afternoon,
            DayPart::Evening,
            DayPart::Evening,
            DayPart::LateNight
        ]
    );
}

#[test]
fn a_long_absence_is_acknowledged_once() {
    let b = compose(&inputs(at(2026, 10, 9, 9, 0), Some(3 * 24 + 2)));
    assert_eq!(b.text, "Good morning, sir. Three days since you were last here. Nothing moved while you were away.");
    let b = compose(&inputs(at(2026, 10, 9, 9, 0), Some(36)));
    assert!(!b.text.contains("since you were last here"), "a day and a half is not an absence worth naming");
}

#[test]
fn discovery_offers_are_one_short_question_with_no_path_spoken() {
    let mut inp = inputs(at(2026, 10, 9, 19, 5), Some(5));
    inp.offers = vec![offer(r"C:\Users\matth\Projects\PruningResearch\valiance", 2)];
    let b = compose(&inp);
    assert_eq!(b.text, r"Good evening, sir. Nothing moved while you were away. PruningResearch\valiance has live runs — shall I watch it?");
    assert_eq!(b.spoken, "Good evening, sir. Nothing moved while you were away. The valiance folder has live runs, shall I watch it?");
    assert_eq!(b.offers, inp.offers);

    inp.offers = vec![offer(r"C:\Users\matth\Projects\PruningResearch\valiance", 1)];
    assert!(compose(&inp).text.ends_with(r"PruningResearch\valiance has a live run — shall I watch it?"));
    inp.offers = vec![offer("/home/m/work/sweeps/lr", 0), offer(r"D:\exp\ablate", 3)];
    let b = compose(&inp);
    assert!(b.text.ends_with(r"sweeps/lr has fresh runs and exp\ablate has live runs — shall I watch them?"), "{}", b.text);
    assert!(b.spoken.ends_with("The lr folder has fresh runs and the ablate folder has live runs, shall I watch them?"), "{}", b.spoken);
    assert_eq!(b.text.matches('?').count(), 1, "never more than one question");
}

#[test]
fn without_the_sentinel_the_brief_does_not_claim_a_quiet_night() {
    let mut inp = inputs(at(2026, 10, 9, 7, 30), Some(10));
    inp.sentinel = false;
    assert_eq!(compose(&inp).text, "Good morning, sir. The Sentinel is not running, so I have nothing on your runs.");
    inp.away = None;
    assert_eq!(compose(&inp).text, "Good morning, sir. The Sentinel is not running, so I have nothing on your runs.");
}

#[test]
fn long_lists_say_how_many_more() {
    let texts: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
    assert_eq!(join_list(&texts), "a, b, c, and 2 more");
    assert_eq!(join_list(&texts[..2]), "a and b");
    assert_eq!(join_list(&["x (1, 2)".into(), "y".into(), "z".into()]), "x (1, 2); y; and z");
}

#[test]
fn should_brief_follows_the_away_rule() {
    assert!(should_brief(None, true));
    assert!(should_brief(Some(AWAY_THRESHOLD), false));
    assert!(!should_brief(Some(AWAY_THRESHOLD - Duration::from_secs(1)), false));
    assert!(should_brief(Some(Duration::from_secs(60)), true), "first session today");
}

// ---------------------------------------------------------------- spoken form

#[test]
fn the_spoken_form_is_for_the_ear() {
    assert_eq!(
        spoken("**Done**: see `C:\\runs\\wnr5\\train.log` and [the dashboard](https://example.com/x)."),
        "Done: see train.log and the dashboard."
    );
    assert_eq!(spoken("Read https://example.com/a?b=c now."), "Read a link now.");
    assert_eq!(spoken("audit3_marlowe loss 1.21834, top1 0.875 → 0.9"), "audit3 marlowe loss 1.22, top1 0.875 to 0.9");
    assert_eq!(spoken("step 12034 of 1500000 at 04:12"), "step about 12 thousand of about 1.5 million at 04:12");
    assert_eq!(spoken("- one\n- two (3/4 done) · 87%"), "one two, 3/4 done, 87 percent");
    assert_eq!(spoken("wnr-p5 at layer 9; 2026 runs"), "wnr-p5 at layer 9, 2026 runs");
    // A slash between words is "or", not a path; real paths still shrink to their leaf.
    assert_eq!(spoken("x logged NaN/inf at 05:19:34"), "x logged NaN or inf at 05:19:34");
    assert_eq!(spoken("train.log: loss went NaN/inf"), "train.log: loss went NaN or inf");
    assert_eq!(spoken("see runs/wnr5/train.log and /tmp/x"), "see train.log and x");
    // Log decoration is not read out.
    assert_eq!(spoken("=== stage pickfinal finished ==="), "stage pickfinal finished");
}

// ---------------------------------------------------------------- polish

enum Script {
    Say(&'static str),
    Fail,
    /// Blocks until cancelled, recording that it saw the cancel.
    Hang,
}

struct Scripted {
    script: Mutex<Option<Script>>,
    saw_cancel: AtomicBool,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn new(s: Script) -> Arc<Self> {
        Arc::new(Self { script: Mutex::new(Some(s)), saw_cancel: AtomicBool::new(false), requests: Mutex::new(Vec::new()) })
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::bare(ProviderKind::Ollama, "quick")])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        match self.script.lock().unwrap().take().expect("one call") {
            Script::Say(t) => {
                let evs = vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)];
                for e in &evs {
                    on(e.clone());
                }
                Ok(fold_events(&evs))
            }
            Script::Fail => Err(ProviderError::auth("over the spend limit")),
            Script::Hang => {
                let end = Instant::now() + Duration::from_secs(5);
                while !cancel.is_cancelled() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.saw_cancel.store(cancel.is_cancelled(), Ordering::SeqCst);
                Err(ProviderError::cancelled())
            }
        }
    }
}

fn draft() -> (BriefInputs, Brief) {
    let mut inp = valiance_morning();
    inp.offers.clear();
    let b = compose(&inp);
    assert!(b.text.starts_with("Good morning, sir. Valiance finished at 06:54"), "{}", b.text);
    (inp, b)
}

fn system() -> Vec<SystemBlock> {
    vec![SystemBlock { text: "persona".into(), cache: true }]
}

const POLISHED: &str = "Good morning, sir. Valiance shipped at 06:54: the wnr-final truewidth Q4_K_S GGUF is written and checked. \
                        Its audit beat wnr7 on every suite, code KL down to 0.071 with 99.1 percent retained.";

#[test]
fn a_polished_brief_is_used_when_it_keeps_to_the_facts() {
    let (inp, b) = draft();
    let p = Scripted::new(Script::Say(POLISHED));
    let req = polish_request("quick", system(), &inp, &b);
    let got = polish(p.clone(), req, &b.text, &[], Duration::from_secs(2));
    assert_eq!(got.as_deref(), Some(POLISHED));
    let sent = p.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.model, "quick");
    assert_eq!(sent.effort, Effort::Off);
    assert_eq!(sent.system, system());
    let prompt = sent.messages[0].joined_text();
    assert!(prompt.contains(&b.text) && prompt.contains("- Its audit beat wnr7 on all four suites;"), "{prompt}");
    assert!(prompt.contains("Local time: Friday 08:14. Away: 9 h."), "{prompt}");
}

#[test]
fn a_polish_that_invents_a_number_or_breaks_register_falls_back() {
    let (inp, b) = draft();
    for bad in [
        "Good morning, sir. Valiance finished at 06:54; code KL is down to 0.068.",
        "Good morning, sir! Valiance finished at 06:54.",
        "- Valiance finished at 06:54",
        "Certainly, sir. Valiance finished at 06:54.",
        "   ",
    ] {
        let p = Scripted::new(Script::Say(bad));
        let got = polish(p, polish_request("quick", system(), &inp, &b), &b.text, &[], Duration::from_secs(2));
        assert_eq!(got, None, "{bad:?} must fall back to the deterministic text");
    }
}

#[test]
fn a_failed_or_slow_polish_falls_back_and_cancels_the_call() {
    let (inp, b) = draft();
    let failing = Scripted::new(Script::Fail);
    assert_eq!(polish(failing, polish_request("quick", system(), &inp, &b), &b.text, &[], Duration::from_secs(2)), None);

    let slow = Scripted::new(Script::Hang);
    let started = Instant::now();
    let got = polish(slow.clone(), polish_request("quick", system(), &inp, &b), &b.text, &[], Duration::from_millis(150));
    assert_eq!(got, None);
    assert!(started.elapsed() < Duration::from_secs(2), "the deadline holds: {:?}", started.elapsed());
    let end = Instant::now() + Duration::from_secs(3);
    while !slow.saw_cancel.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(slow.saw_cancel.load(Ordering::SeqCst), "the abandoned call is cancelled, not left running");
}

#[test]
fn polish_acceptance_compares_numbers_exactly() {
    let draft = "wnr5 finished at 04:12 (loss 1.218).";
    assert!(accept_polish(draft, &[], "The wnr5 run finished at 04:12, loss 1.218."));
    assert!(!accept_polish(draft, &[], "The wnr5 run finished at 4:12, loss 1.218."), "04:12 written differently is not trusted");
    assert!(!accept_polish(draft, &[], "wnr6 finished at 04:12, loss 1.218."));
    assert!(!accept_polish(draft, &[], &"word ".repeat(200)));
}

#[test]
fn a_local_model_is_not_loaded_onto_a_busy_gpu() {
    use marlowe_proto::{GpuDevice, GpuReport};
    let device = |util_pct, mem_used_mb| GpuDevice {
        index: 0,
        name: "RTX 4080".into(),
        util_pct,
        mem_used_mb,
        mem_total_mb: 16_376,
        temp_c: None,
        processes: Vec::new(),
    };
    let idle = GpuReport { devices: vec![device(2, 900)], unavailable: None };
    let busy = GpuReport { devices: vec![device(92, 6_700)], unavailable: None };
    let live = vec![run(r"C:\r\a", "a", RunStatus::Alive, 0)];
    assert!(polish_allowed(ProviderKind::OpenRouter, false, &live, &busy), "a remote model never touches the GPU");
    assert!(!polish_allowed(ProviderKind::Ollama, false, &[], &idle), "local polish is off by default");
    assert!(!polish_allowed(ProviderKind::Ollama, true, &[], &busy));
    assert!(!polish_allowed(ProviderKind::LocalGguf, true, &live, &idle), "a live run keeps the GPU");
    assert!(!polish_allowed(ProviderKind::Ollama, true, &[], &GpuReport::default()), "an unknown GPU is not idle");
    assert!(polish_allowed(ProviderKind::Ollama, true, &[], &idle));
}

#[test]
fn a_polish_that_breaks_persona_or_drops_an_alarm_falls_back() {
    let draft = "Good morning, sir. Overnight, wnr5 finished at 04:12. One thing needs you: wnr6 stalled (no heartbeat for 42 min).";
    let attention = vec!["wnr6 stalled (no heartbeat for 42 min)".to_string()];
    let ok = "Good morning, sir. wnr5 finished at 04:12, and wnr6 has stalled with no heartbeat for 42 min.";
    assert!(accept_polish(draft, &attention, ok));
    for bad in [
        // Dropped the alarm.
        "Good morning, sir. wnr5 finished at 04:12.",
        // Kept the name, lost the number.
        "Good morning, sir. wnr5 finished at 04:12, and wnr6 has stalled.",
        // Persona v3 words to avoid, beyond the old short list.
        "Good morning, sir. Honestly, wnr5 finished at 04:12, and wnr6 has stalled with no heartbeat for 42 min.",
        "Good morning, sir. wnr5 finished at 04:12 and wnr6 stalled with no heartbeat for 42 min. Let me know.",
        "Good morning, sir. To be clear, wnr5 finished at 04:12 and wnr6 stalled with no heartbeat for 42 min.",
        // Emoji.
        "Good morning, sir \u{1F319}. wnr5 finished at 04:12 and wnr6 stalled with no heartbeat for 42 min.",
        "Good morning, sir. wnr5 finished at 04:12 \u{2705} and wnr6 stalled with no heartbeat for 42 min.",
        // More than five sentences.
        "Good morning, sir. wnr5 finished. It was at 04:12. Then wnr6 stalled. No heartbeat for 42 min. That is all. Quiet otherwise.",
    ] {
        assert!(!accept_polish(draft, &attention, bad), "{bad:?}");
    }
    // An attention item the draft did not state (beyond "and N more") is not required.
    assert!(accept_polish(draft, &["wnr9 failed at 03:00".to_string()], ok));
    // A banned word that is part of a fact is the fact.
    assert!(accept_polish("robust-ft finished at 04:12.", &[], "The robust-ft run finished at 04:12."));
    assert!(banned_phrases().iter().any(|b| b == "i'd be happy to"), "parsed from persona v3: {:?}", banned_phrases());
    assert!(banned_phrases().iter().any(|b| b == "it's worth noting that"));
}
