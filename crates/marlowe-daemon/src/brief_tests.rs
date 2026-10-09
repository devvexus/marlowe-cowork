//! Brief composition against a fixed clock and synthetic notices; the polish path against a
//! scripted provider. No daemon, no network, no inference.

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
const RUNS: &str = r"C:\Users\m\Projects\PruningResearch\valiance\runs";

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().expect("an unambiguous local time")
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

fn memory(beliefs: u64, episodes: u64) -> MemoryStatsReport {
    MemoryStatsReport { beliefs, episodes, ..Default::default() }
}

fn sentences(text: &str) -> usize {
    sentence_count(text)
}

fn offer(path: &str, live_runs: u32) -> WatchCandidate {
    WatchCandidate { path: path.into(), reason: "heartbeat.json, 3 logs".into(), modified_ms: 0, live_runs }
}

// ---------------------------------------------------------------- composition

#[test]
fn a_morning_after_nine_hours_leads_with_what_finished_then_what_needs_you() {
    let now = at(2026, 10, 9, 8, 14);
    let dir5 = format!(r"{RUNS}\wnr5");
    let dir6 = format!(r"{RUNS}\wnr6");
    let mut inp = inputs(now, Some(9));
    let finished_at = at(2026, 10, 9, 4, 12).timestamp_millis();
    inp.notices = vec![
        notice(NoticeSource::Run, Severity::Attention, "stalled", "marlowe-valiance-22b-wnr6 stalled at layer 9", "no heartbeat for 42 min", &dir6, finished_at + 1000),
        notice(NoticeSource::Run, Severity::Good, "finished", "marlowe-valiance-22b-wnr5 finished", "stage wnr-p5, layer 9", &dir5, finished_at),
        notice(
            NoticeSource::Metric,
            Severity::Good,
            "best",
            "audit3 wnr4 set a new best",
            "kl 0.0803, was 0.0912",
            r"C:\Users\m\Projects\PruningResearch\valiance\audit3_marlowe-valiance-22b-wnr4.json",
            finished_at - 1000,
        ),
    ];
    // The same two runs as the Sentinel's state reports them: covered by their notices.
    let mut stalled = run(&dir6, "marlowe-valiance-22b-wnr6", RunStatus::Stalled, finished_at);
    stalled.layer = Some(9);
    stalled.heartbeat_ms = Some(now.timestamp_millis() - 42 * 60_000);
    inp.runs = vec![run(&dir5, "marlowe-valiance-22b-wnr5", RunStatus::Finished, finished_at), stalled];
    inp.memory = Some(memory(390, 12));

    let b = compose(&inp);
    assert_eq!(
        b.text,
        "Good morning, sir. Overnight, marlowe-valiance-22b-wnr5 finished at 04:12 (stage wnr-p5, layer 9) and \
         audit3 wnr4 set a new best (kl 0.0803, was 0.0912). One thing needs you: marlowe-valiance-22b-wnr6 \
         stalled at layer 9 (no heartbeat for 42 min). Memory holds 390 beliefs and 12 episodes."
    );
    let kinds: Vec<BriefItemKind> = b.items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [BriefItemKind::Finished, BriefItemKind::Finished, BriefItemKind::Attention, BriefItemKind::Memory],
        "each run once: the notices cover the run states"
    );
    assert!(!b.is_quiet());
    assert_eq!(
        b.spoken,
        "Good morning, sir. Overnight, marlowe-valiance-22b-wnr5 finished at 04:12, stage wnr-p5, layer 9, and \
         audit3 wnr4 set a new best, kl 0.0803, was 0.0912. One thing needs you: marlowe-valiance-22b-wnr6 \
         stalled at layer 9, no heartbeat for 42 min. Memory holds 390 beliefs and 12 episodes."
    );
}

#[test]
fn run_states_without_notices_still_brief_and_live_runs_are_listed_last() {
    let now = at(2026, 10, 9, 14, 0);
    let mut inp = inputs(now, Some(3));
    let mut alive = run(&format!(r"{RUNS}\wnr7"), "wnr7", RunStatus::Alive, 0);
    alive.stage = Some("wnr-p2".into());
    alive.layer = Some(3);
    let mut stalled = run(&format!(r"{RUNS}\wnr6"), "wnr6", RunStatus::Stalled, 0);
    stalled.layer = Some(9);
    stalled.heartbeat_ms = Some(now.timestamp_millis() - (2 * 60 + 5) * 60_000);
    let mut failed = run(&format!(r"{RUNS}\wnr8"), "wnr8", RunStatus::Failed, at(2026, 10, 9, 12, 30).timestamp_millis());
    failed.last_line = Some("torch.cuda.OutOfMemoryError: CUDA out of memory".into());
    inp.runs = vec![alive, stalled, failed];
    let b = compose(&inp);
    assert_eq!(
        b.text,
        "Good afternoon, sir. Two things need you: wnr6 has stalled at layer 9 (no heartbeat for 2 h 5 min) and \
         wnr8 failed at 12:30 (torch.cuda.OutOfMemoryError: CUDA out of memory). wnr7 (stage wnr-p2, layer 3) is \
         still running."
    );
}

#[test]
fn a_quiet_night_is_one_line() {
    let mut inp = inputs(at(2026, 10, 9, 7, 30), Some(10));
    inp.memory = Some(memory(390, 12));
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Nothing moved while you were away.");
    assert_eq!(b.spoken, b.text);
    assert!(b.is_quiet());
    assert_eq!(b.items.len(), 1, "the memory line is still an item for the board");
    // A first-ever session has no "away".
    let first = compose(&inputs(at(2026, 10, 9, 7, 30), None));
    assert_eq!(first.text, "Good morning, sir. Nothing to report yet.");
}

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
fn discovery_offers_are_phrased_as_one_question() {
    let mut inp = inputs(at(2026, 10, 9, 19, 5), Some(5));
    inp.offers = vec![offer(r"C:\Users\matth\Projects\PruningResearch\valiance", 2)];
    let b = compose(&inp);
    assert_eq!(
        b.text,
        r"Good evening, sir. Nothing moved while you were away. I can watch PruningResearch\valiance — it has live runs. Watch it?"
    );
    assert_eq!(b.spoken, "Good evening, sir. Nothing moved while you were away. I can watch valiance, it has live runs. Watch it?");
    assert_eq!(b.offers, inp.offers);

    inp.offers = vec![offer(r"C:\Users\matth\Projects\PruningResearch\valiance", 1)];
    assert!(compose(&inp).text.ends_with(r"I can watch PruningResearch\valiance — it has a live run. Watch it?"));
    inp.offers = vec![offer("/home/m/work/sweeps/lr", 0), offer(r"D:\exp\ablate", 3)];
    assert!(compose(&inp).text.ends_with(r"I can watch sweeps/lr (recent runs) and exp\ablate (live runs). Watch them?"));
    assert_eq!(compose(&inp).text.matches('?').count(), 1, "never more than one question");
}

#[test]
fn a_busy_brief_keeps_to_five_sentences_by_priority() {
    let now = at(2026, 10, 9, 8, 0);
    let mut inp = inputs(now, Some(9));
    let t = now.timestamp_millis();
    inp.notices = vec![
        notice(NoticeSource::Trainer, Severity::Good, "finished", "ft-a finished", "eval_loss 1.82", r"C:\r\a", t),
        notice(NoticeSource::Log, Severity::Alarm, "nan", "NaN loss in ft-b", "step 1200", r"C:\r\b\train.log", t),
        notice(NoticeSource::Ollama, Severity::Info, "new_model", "new model marlowe-x", "", "marlowe-x:latest", t),
        notice(NoticeSource::Gpu, Severity::Info, "hot", "GPU at 83 C", "", "gpu0", t),
    ];
    inp.runs = vec![run(r"C:\r\c", "ft-c", RunStatus::Alive, 0), run(r"C:\r\d", "ft-d", RunStatus::Alive, 0)];
    inp.new_models = vec!["marlowe-x:latest".into(), "qwen3:8b".into()];
    inp.git = vec![(
        r"C:\Users\m\Projects\MarloweCowork".into(),
        GitInfo { branch: "main".into(), dirty: 3, new_commits: 2, ..Default::default() },
    )];
    inp.memory = Some(memory(4212, 318));
    inp.offers = vec![offer(r"C:\Users\m\Projects\PruningResearch\valiance", 2)];

    let b = compose(&inp);
    assert_eq!(
        b.text,
        "Good morning, sir. Overnight, ft-a finished at 08:00 (eval_loss 1.82). One thing needs you: NaN loss in \
         ft-b (step 1200). Still running: ft-c and ft-d. New in Ollama: marlowe-x:latest and qwen3:8b."
    );
    assert_eq!(b.offers.len(), 1, "an offer that does not fit the sentences still reaches the client");
    assert!(sentences(&b.text) <= MAX_SENTENCES);
    let count = |k| b.items.iter().filter(|i| i.kind == k).count();
    assert_eq!(count(BriefItemKind::Model), 2, "the notice and the state name marlowe-x once");
    assert_eq!(count(BriefItemKind::Git), 1);
    assert_eq!(count(BriefItemKind::Memory), 1);
    assert!(b.items.iter().all(|i| !i.text.contains("GPU")), "GPU chatter stays on the board");
    let git = b.items.iter().find(|i| i.kind == BriefItemKind::Git).unwrap();
    assert_eq!(git.text, "MarloweCowork: 2 new commits on main, 3 uncommitted");

    // With fewer sections the two-sentence offer fits, after the priorities above it.
    inp.notices.truncate(1);
    inp.runs.clear();
    assert_eq!(
        compose(&inp).text,
        r"Good morning, sir. Overnight, ft-a finished at 08:00 (eval_loss 1.82). New in Ollama: marlowe-x:latest and qwen3:8b. I can watch PruningResearch\valiance — it has live runs. Watch it?"
    );

    // Without the offer, models, git and memory read in canonical order.
    inp.offers.clear();
    inp.runs.clear();
    let b = compose(&inp);
    assert_eq!(
        b.text,
        "Good morning, sir. Overnight, ft-a finished at 08:00 (eval_loss 1.82). New in Ollama: marlowe-x:latest and \
         qwen3:8b. MarloweCowork: 2 new commits on main, 3 uncommitted. Memory holds 4212 beliefs and 318 episodes."
    );
}

#[test]
fn a_line_teed_into_two_logs_is_said_once_without_its_timestamp_twice() {
    // Shape of the live night1 case: stage_27b.log and wnr_27b.log both end a stage.
    let now = at(2026, 10, 9, 6, 35);
    let mut inp = inputs(now, Some(9));
    let line = "[06:34:29] === stage pickfinal finished";
    let ms = now.timestamp_millis() - 60_000;
    for log in ["wnr_27b.log", "stage_27b.log"] {
        let subject = format!(r"{RUNS}\night1\{log}");
        inp.notices.push(notice(NoticeSource::Log, Severity::Good, "completion", &format!("{log}: completed at 06:34:29"), line, &subject, ms));
    }
    let b = compose(&inp);
    assert_eq!(b.text.matches("pickfinal").count(), 1, "{}", b.text);
    assert!(b.text.contains("wnr_27b.log: completed at 06:34:29 (stage pickfinal finished)"), "{}", b.text);
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
    let now = at(2026, 10, 9, 8, 14);
    let mut inp = inputs(now, Some(9));
    inp.notices = vec![notice(
        NoticeSource::Run,
        Severity::Good,
        "finished",
        "wnr5 finished",
        "eval loss 1.82",
        r"C:\r\wnr5",
        at(2026, 10, 9, 4, 12).timestamp_millis(),
    )];
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. Overnight, wnr5 finished at 04:12 (eval loss 1.82).");
    (inp, b)
}

fn system() -> Vec<SystemBlock> {
    vec![SystemBlock { text: "persona".into(), cache: true }]
}

#[test]
fn a_polished_brief_is_used_when_it_keeps_to_the_facts() {
    let (inp, b) = draft();
    let p = Scripted::new(Script::Say("Good morning, sir. The wnr5 run finished at 04:12 with eval loss 1.82."));
    let req = polish_request("quick", system(), &inp, &b);
    let got = polish(p.clone(), req, &b.text, &[], Duration::from_secs(2));
    assert_eq!(got.as_deref(), Some("Good morning, sir. The wnr5 run finished at 04:12 with eval loss 1.82."));
    let sent = p.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.model, "quick");
    assert_eq!(sent.effort, Effort::Off);
    assert_eq!(sent.system, system());
    let prompt = sent.messages[0].joined_text();
    assert!(prompt.contains(&b.text) && prompt.contains("- wnr5 finished at 04:12 (eval loss 1.82)"), "{prompt}");
    assert!(prompt.contains("Local time: Friday 08:14. Away: 9 h."), "{prompt}");
}

#[test]
fn a_polish_that_invents_a_number_or_breaks_register_falls_back() {
    let (inp, b) = draft();
    for bad in [
        "Good morning, sir. wnr5 finished at 04:12 with eval loss 1.79.",
        "Good morning, sir! wnr5 finished at 04:12.",
        "- wnr5 finished at 04:12",
        "Certainly, sir. wnr5 finished at 04:12.",
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

// ---------------------------------------------------------------- honesty

#[test]
fn a_stall_the_run_has_moved_on_from_is_not_something_needing_you() {
    let now = at(2026, 10, 9, 8, 14);
    let dir6 = format!(r"{RUNS}\wnr6");
    let t = at(2026, 10, 9, 2, 0).timestamp_millis();
    for status in [RunStatus::Finished, RunStatus::Alive] {
        let mut inp = inputs(now, Some(9));
        inp.notices = vec![notice(NoticeSource::Run, Severity::Attention, "stalled", "wnr6 stalled", "no heartbeat for 42 min", &dir6, t)];
        inp.runs = vec![run(&dir6, "wnr6", status, at(2026, 10, 9, 4, 12).timestamp_millis())];
        let b = compose(&inp);
        assert!(b.items.iter().all(|i| i.kind != BriefItemKind::Attention), "{status:?}: {}", b.text);
        assert!(!b.text.contains("stalled"), "{}", b.text);
    }
    // Still stalled: it still needs you.
    let mut inp = inputs(now, Some(9));
    inp.notices = vec![notice(NoticeSource::Run, Severity::Attention, "stalled", "wnr6 stalled", "no heartbeat for 42 min", &dir6, t)];
    inp.runs = vec![run(&dir6, "wnr6", RunStatus::Stalled, t)];
    assert!(compose(&inp).text.contains("One thing needs you: wnr6 stalled"));
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
fn plain_new_results_stay_on_the_board_and_bests_keep_their_numbers() {
    let now = at(2026, 10, 9, 8, 0);
    let mut inp = inputs(now, Some(9));
    let t = now.timestamp_millis();
    let json = |s: &str| format!(r"C:\Users\m\Projects\PruningResearch\valiance\scratch\{s}.json");
    let long = "code.kl 0.0803 (wnr7 0.0912, -0.0109, best) · code.top1 0.881 (wnr7 0.874, +0.007, best) · \
                code.loss 1.21 (wnr7 1.25, -0.04) · thinking.kl 0.0827 (wnr7 0.0901, -0.0074)";
    inp.notices = vec![
        notice(NoticeSource::Metric, Severity::Info, "result", "audit3_x: new results", "kl 0.09", &json("audit3_x"), t),
        notice(NoticeSource::Metric, Severity::Good, "result", "audit2_final: best so far on 4 metrics", long, &json("audit2_final"), t),
    ];
    let b = compose(&inp);
    assert!(!b.text.contains("new results"), "{}", b.text);
    assert_eq!(
        b.text,
        "Good morning, sir. Overnight, audit2_final: best so far on 4 metrics (code.kl 0.0803 (wnr7 0.0912, -0.0109, best) · \
         code.top1 0.881 (wnr7 0.874, +0.007, best))."
    );
    assert_eq!(fit_detail("a · b"), Some("a · b".into()));
    assert_eq!(fit_detail(&"x".repeat(DETAIL_MAX + 1)), None);
    let three = format!("{} · {} · {}", "a".repeat(40), "b".repeat(40), "c".repeat(40));
    assert_eq!(fit_detail(&three), Some(format!("{} · {}", "a".repeat(40), "b".repeat(40))));
}

#[test]
fn a_stage_ending_inside_a_live_run_is_not_the_run_finishing() {
    let now = at(2026, 10, 9, 6, 40);
    let dir = format!(r"{RUNS}\night1");
    let mut inp = inputs(now, Some(9));
    let ms = now.timestamp_millis() - 240_000;
    inp.notices = vec![notice(
        NoticeSource::Log,
        Severity::Good,
        "completion",
        "stage_27b.log: completed at 06:35:48",
        "[06:35:48] === stage prune finished",
        &format!(r"{dir}\stage_27b.log"),
        ms,
    )];
    let mut live = run(&dir, "wnr-final", RunStatus::Alive, 0);
    live.stage = Some("audit".into());
    live.layer = Some(32);
    inp.runs = vec![live];
    let b = compose(&inp);
    assert_eq!(b.text, "Good morning, sir. wnr-final (stage audit, layer 32; stage prune finished at 06:35:48) is still running.");
    assert!(b.items.iter().all(|i| i.kind != BriefItemKind::Finished));
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
