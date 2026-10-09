//! The Wake brief: what changed while the user was away, in persona v3's register.
//!
//! On a TUI client's `Hello`, when the user has been away >= [`AWAY_THRESHOLD`] (last client
//! disconnect / last activity in `cowork.db`) or it is the first session today, the daemon
//! gathers [`BriefInputs`] from the Sentinel, the store and the Mind, renders a deterministic
//! [`Brief`] with [`compose`], optionally polishes it with one Quick/Conductor call using
//! `persona/blocks/briefing.md` (timeout [`POLISH_TIMEOUT`]; any failure keeps the deterministic
//! text), stores it as the first assistant message of the day's session, and sends
//! `Event::Brief` (speaking [`Brief::spoken`] when voice is on and outside quiet hours).
//!
//! This module is pure composition; the daemon side lives in `brief_ops`.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Timelike};
use marlowe_model::{CancelToken, ChatRequest, Effort, Message, Provider, ProviderKind, SystemBlock};
use marlowe_proto::{
    BriefItem, BriefItemKind, GitInfo, GpuReport, MemoryStatsReport, Notice, NoticeSource, RunInfo, RunStatus, Severity,
    WatchCandidate,
};

use crate::daemon::Core;

pub(crate) use crate::brief_ops::Presence;

/// Away at least this long before a brief is offered.
pub const AWAY_THRESHOLD: Duration = Duration::from_secs(2 * 3600);

/// The optional LLM polish gives up after this long.
pub const POLISH_TIMEOUT: Duration = Duration::from_secs(6);

/// A brief is at most this many sentences, greeting included.
const MAX_SENTENCES: usize = 5;

/// Items listed per sentence before "and N more".
const MAX_LISTED: usize = 3;

/// A notice detail longer than this stays out of the sentence (it is still in `items`).
const DETAIL_MAX: usize = 100;

/// Away this long in the morning reads as "overnight".
const OVERNIGHT: Duration = Duration::from_secs(6 * 3600);

/// Output cap for the polish call.
const POLISH_MAX_TOKENS: u32 = 400;

/// Everything a brief is composed from. Plain data, so composition is testable without a daemon.
#[derive(Debug, Clone)]
pub struct BriefInputs {
    /// Local wall-clock time now.
    pub now: DateTime<Local>,
    /// Time since the last client disconnect / last activity; `None` on a first-ever session.
    pub away: Option<Duration>,
    /// No session has started today yet.
    pub first_today: bool,
    /// Address form from the profile (`sir`); `None` addresses no one.
    pub address: Option<String>,
    /// Notices since the user was last seen, ranked.
    pub notices: Vec<Notice>,
    /// Current run states (finished/failed since last seen, and live ones).
    pub runs: Vec<RunInfo>,
    /// Ollama tags new since last seen.
    pub new_models: Vec<String>,
    /// Active workspaces' git state: `(workspace, info)`.
    pub git: Vec<(String, GitInfo)>,
    pub memory: Option<MemoryStatsReport>,
    /// Discovery candidates to offer.
    pub offers: Vec<WatchCandidate>,
    /// The Sentinel answered. False when it is not running (or failed to report): the brief
    /// then says it knows nothing about runs rather than that nothing moved.
    pub sentinel: bool,
}

/// A composed brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// Written form: 2–5 sentences, numbers flat and exact; one line when nothing happened.
    pub text: String,
    /// Ear-friendly form per `persona/blocks/voice.md`.
    pub spoken: String,
    pub items: Vec<BriefItem>,
    pub offers: Vec<WatchCandidate>,
}

impl Brief {
    /// Nothing happened: no items beyond the memory line.
    pub fn is_quiet(&self) -> bool {
        self.items.iter().all(|i| i.kind == BriefItemKind::Memory)
    }
}

/// Whether to brief on this Hello.
pub fn should_brief(away: Option<Duration>, first_today: bool) -> bool {
    first_today || away.is_some_and(|a| a >= AWAY_THRESHOLD)
}

/// The part of day for a local hour: morning 05–11, afternoon 12–17, evening 18–22, late night
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayPart {
    Morning,
    Afternoon,
    Evening,
    LateNight,
}

impl DayPart {
    pub fn of(now: &DateTime<Local>) -> DayPart {
        match now.hour() {
            5..=11 => DayPart::Morning,
            12..=17 => DayPart::Afternoon,
            18..=22 => DayPart::Evening,
            _ => DayPart::LateNight,
        }
    }
}

/// `Good morning, sir.` / `Good evening.`; late night is a dry, short line.
pub fn greeting(part: DayPart, address: Option<&str>) -> String {
    let head = match part {
        DayPart::Morning => "Good morning",
        DayPart::Afternoon => "Good afternoon",
        DayPart::Evening => "Good evening",
        DayPart::LateNight => "Up late",
    };
    match address.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{head}, {a}."),
        None => format!("{head}."),
    }
}

// ---------------------------------------------------------------- composition

/// Renders the deterministic brief.
pub fn compose(inputs: &BriefInputs) -> Brief {
    let items = collect_items(inputs);
    let part = DayPart::of(&inputs.now);
    let mut sentences = vec![greeting(part, inputs.address.as_deref())];
    if let Some(a) = absence_line(inputs.away) {
        sentences.push(a);
    }
    let offer = offer_sentence(&inputs.offers);
    let quiet = items.iter().all(|i| i.kind == BriefItemKind::Memory);
    if quiet {
        let line = if !inputs.sentinel {
            "The Sentinel is not running, so I have nothing on your runs."
        } else if inputs.away.is_none() {
            "Nothing to report yet."
        } else {
            "Nothing moved while you were away."
        };
        sentences.push(line.into());
        sentences.extend(offer);
    } else {
        let room = MAX_SENTENCES.saturating_sub(sentences.len());
        sentences.extend(body(inputs, part, &items, offer, room));
    }
    let text = sentences.join(" ");
    Brief { spoken: spoken(&text), text, items, offers: inputs.offers.clone() }
}

/// What a run's status says in a brief.
fn run_kind(status: RunStatus) -> BriefItemKind {
    match status {
        RunStatus::Alive => BriefItemKind::Running,
        RunStatus::Stalled | RunStatus::Failed => BriefItemKind::Attention,
        RunStatus::Finished | RunStatus::Ended => BriefItemKind::Finished,
    }
}

fn is_end_kind(kind: &str) -> bool {
    matches!(kind, "finished" | "completed" | "complete" | "done" | "ended" | "end")
}

/// Where a notice goes in a brief; `None` leaves it out (GPU chatter, discovery, run starts).
fn notice_kind(n: &Notice) -> Option<BriefItemKind> {
    if n.severity.is_urgent() {
        return Some(BriefItemKind::Attention);
    }
    match n.source {
        NoticeSource::Ollama => Some(BriefItemKind::Model),
        NoticeSource::Git => Some(BriefItemKind::Git),
        // A plain "new results" stays on the board; only a best-so-far is news.
        NoticeSource::Metric => (n.severity == Severity::Good).then_some(BriefItemKind::Finished),
        NoticeSource::Trainer => Some(BriefItemKind::Finished),
        NoticeSource::Run | NoticeSource::Log => {
            (n.severity == Severity::Good || is_end_kind(&n.kind)).then_some(BriefItemKind::Finished)
        }
        NoticeSource::Gpu | NoticeSource::Discovery => None,
    }
}

/// `HH:MM` local for an epoch-ms instant.
fn clock(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%H:%M").to_string(),
        None => "?".into(),
    }
}

fn notice_text(n: &Notice) -> String {
    let mut t = n.title.trim().to_string();
    let ends = matches!(n.source, NoticeSource::Run | NoticeSource::Log | NoticeSource::Trainer) && is_end_kind(&n.kind);
    if ends && !t.contains(" at ") {
        t.push_str(&format!(" at {}", clock(n.at_ms)));
    }
    // The title already says when; drop the line's own `[hh:mm:ss]` prefix.
    let d = if t.contains(" at ") || n.source == NoticeSource::Log { log_line_body(&n.detail) } else { n.detail.trim().to_string() };
    if let Some(d) = fit_detail(&d) {
        t.push_str(&format!(" ({d})"));
    }
    t
}

/// A detail that fits [`DETAIL_MAX`]: whole, else its leading ` · `-separated parts that fit
/// (a metric notice puts its bests first), else nothing.
fn fit_detail(d: &str) -> Option<String> {
    if d.is_empty() {
        return None;
    }
    if d.chars().count() <= DETAIL_MAX {
        return Some(d.to_string());
    }
    let mut kept = String::new();
    for part in d.split(" · ").map(str::trim).filter(|p| !p.is_empty()) {
        let next = if kept.is_empty() { part.to_string() } else { format!("{kept} · {part}") };
        if next.chars().count() > DETAIL_MAX {
            break;
        }
        kept = next;
    }
    (!kept.is_empty()).then_some(kept)
}

/// `stage wnr-p5, layer 9` / `step 1200, loss 1.218`.
fn run_progress(r: &RunInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(s) = r.stage.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(format!("stage {}", s.trim()));
    }
    if let Some(l) = r.layer {
        parts.push(format!("layer {l}"));
    }
    if let Some(s) = r.step {
        parts.push(format!("step {s}"));
    }
    if let Some(l) = r.loss {
        parts.push(format!("loss {l}"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// `42 min` / `3 h 10 min`.
fn span(ms: i64) -> String {
    let min = (ms.max(0) / 60_000) as u64;
    match (min / 60, min % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

fn run_item(r: &RunInfo, now_ms: i64) -> BriefItem {
    let paren = |s: Option<String>| s.map(|s| format!(" ({s})")).unwrap_or_default();
    let last = r.last_line.as_deref().map(str::trim).filter(|l| !l.is_empty() && l.chars().count() <= DETAIL_MAX);
    let (severity, text) = match r.status {
        RunStatus::Alive => (Severity::Info, format!("{}{}", r.name, paren(run_progress(r)))),
        RunStatus::Stalled => {
            let at = r.layer.map(|l| format!(" at layer {l}")).unwrap_or_default();
            let quiet = r.heartbeat_ms.map(|h| format!("no heartbeat for {}", span(now_ms - h)));
            (Severity::Attention, format!("{} has stalled{at}{}", r.name, paren(quiet)))
        }
        RunStatus::Finished => {
            (Severity::Good, format!("{} finished at {}{}", r.name, clock(r.since_ms), paren(run_progress(r))))
        }
        RunStatus::Failed => {
            (Severity::Attention, format!("{} failed at {}{}", r.name, clock(r.since_ms), paren(last.map(String::from))))
        }
        RunStatus::Ended => (Severity::Info, format!("{} ended at {}{}", r.name, clock(r.since_ms), paren(last.map(String::from)))),
    };
    BriefItem { kind: run_kind(r.status), severity, text, subject: Some(r.dir.clone()) }
}

/// A log line without its leading `[hh:mm:ss]` and without decoration words (`===`, `-->`).
fn log_line_body(line: &str) -> String {
    let l = line.trim();
    let body = l
        .strip_prefix('[')
        .and_then(|r| r.split_once(']'))
        .filter(|(ts, _)| ts.contains(':'))
        .map_or(l, |(_, rest)| rest.trim());
    let mut words: Vec<&str> = body.split_whitespace().collect();
    while words.first().is_some_and(|w| is_decoration(w)) {
        words.remove(0);
    }
    while words.last().is_some_and(|w| is_decoration(w)) {
        words.pop();
    }
    words.join(" ")
}

/// A word made only of rule characters: `===`, `---`, `-->`, `**`, `#`. A lone `-` or `>` is
/// left alone (a dash, a comparison).
fn is_decoration(w: &str) -> bool {
    w.chars().all(|c| "=-*#~_|>".contains(c)) && (w.chars().count() >= 2 || matches!(w, "=" | "*" | "#" | "~" | "|"))
}

/// True when `subject` is the run directory or a file inside it.
fn within(subject: &str, dir: &str) -> bool {
    subject == dir
        || subject.strip_prefix(dir).is_some_and(|rest| rest.starts_with('\\') || rest.starts_with('/'))
}

/// Last path component of a workspace or repo.
fn leaf(path: &str) -> &str {
    path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(path)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn git_text(workspace: &str, g: &GitInfo) -> String {
    let branch = if g.branch.is_empty() { "a detached HEAD".to_string() } else { g.branch.clone() };
    let mut t = format!("{}: {} on {branch}", leaf(workspace), plural(g.new_commits as u64, "new commit", "new commits"));
    if g.dirty > 0 {
        t.push_str(&format!(", {} uncommitted", g.dirty));
    }
    t
}

fn memory_text(m: &MemoryStatsReport) -> String {
    let recall = match &m.degraded {
        None => "recall online".to_string(),
        Some(_) => "recall degraded".to_string(),
    };
    format!("{}, {}, {recall}", plural(m.beliefs, "belief", "beliefs"), plural(m.episodes, "episode", "episodes"))
}

/// Every brief item, in canonical order: finished, attention, running, models, git, memory.
fn collect_items(inp: &BriefInputs) -> Vec<BriefItem> {
    let now_ms = inp.now.timestamp_millis();
    let mut from_notices: Vec<(BriefItemKind, &Notice)> =
        inp.notices.iter().filter_map(|n| notice_kind(n).map(|k| (k, n))).collect();
    // Alarms first within each kind; stable otherwise (the caller ranked them).
    from_notices.sort_by_key(|(_, n)| std::cmp::Reverse(n.severity));
    // Two logs in one run dir often carry the same line (a stage log and its tee): say it once.
    let mut seen_lines: BTreeSet<(String, String)> = BTreeSet::new();
    from_notices.retain(|(_, n)| {
        if n.source != NoticeSource::Log || n.detail.trim().is_empty() {
            return true;
        }
        let dir = n.subject.rsplit_once(['\\', '/']).map_or("", |(d, _)| d).to_string();
        seen_lines.insert((dir, log_line_body(&n.detail)))
    });
    // A stall or failure the run has since moved on from is history, not something needing you.
    from_notices.retain(|(_, n)| {
        let was = match (n.source, n.kind.as_str()) {
            (NoticeSource::Run, "stalled") => RunStatus::Stalled,
            (NoticeSource::Run, "failed") => RunStatus::Failed,
            _ => return true,
        };
        !inp.runs.iter().any(|r| within(&n.subject, &r.dir) && r.status != was)
    });
    // A log's completion inside a run that is still going is a stage ending, not the run: fold it
    // into the run's "still running" line (ranked newest first, so the first one per run wins).
    let mut stage_done: Vec<(&str, String)> = Vec::new();
    from_notices.retain(|(k, n)| {
        if *k != BriefItemKind::Finished || n.source != NoticeSource::Log {
            return true;
        }
        let Some(r) = inp.runs.iter().find(|r| r.status.is_live() && within(&n.subject, &r.dir)) else { return true };
        let body = log_line_body(&n.detail);
        if !body.is_empty() && !stage_done.iter().any(|(d, _)| *d == r.dir) {
            let when = n.title.rsplit_once(" at ").map_or_else(|| clock(n.at_ms), |(_, t)| t.trim().to_string());
            stage_done.push((r.dir.as_str(), format!("{body} at {when}")));
        }
        false
    });
    let mut items: Vec<BriefItem> = from_notices
        .iter()
        .map(|(k, n)| {
            // A model is named by its tag.
            let text = if *k == BriefItemKind::Model { n.subject.clone() } else { notice_text(n) };
            BriefItem { kind: *k, severity: n.severity, text, subject: Some(n.subject.clone()) }
        })
        .collect();
    for r in &inp.runs {
        let kind = run_kind(r.status);
        let covered = from_notices.iter().any(|(k, n)| *k == kind && within(&n.subject, &r.dir));
        if !covered {
            let mut item = run_item(r, now_ms);
            if r.status == RunStatus::Alive {
                if let Some((_, done)) = stage_done.iter().find(|(d, _)| *d == r.dir) {
                    item.text = match run_progress(r) {
                        Some(p) => format!("{} ({p}; {done})", r.name),
                        None => format!("{} ({done})", r.name),
                    };
                }
            }
            items.push(item);
        }
    }
    let mut models: BTreeSet<&str> = BTreeSet::new();
    for (k, n) in &from_notices {
        if *k == BriefItemKind::Model {
            models.insert(n.subject.as_str());
        }
    }
    for m in &inp.new_models {
        if models.insert(m.as_str()) {
            items.push(BriefItem { kind: BriefItemKind::Model, severity: Severity::Info, text: m.clone(), subject: Some(m.clone()) });
        }
    }
    for (ws, g) in &inp.git {
        let noticed = from_notices.iter().any(|(k, n)| *k == BriefItemKind::Git && (n.subject == *ws || within(ws, &n.subject)));
        if g.new_commits > 0 && !noticed {
            items.push(BriefItem { kind: BriefItemKind::Git, severity: Severity::Info, text: git_text(ws, g), subject: Some(ws.clone()) });
        }
    }
    if let Some(m) = &inp.memory {
        items.push(BriefItem { kind: BriefItemKind::Memory, severity: Severity::Info, text: memory_text(m), subject: None });
    }
    let order = |k: BriefItemKind| match k {
        BriefItemKind::Finished => 0,
        BriefItemKind::Attention => 1,
        BriefItemKind::Running => 2,
        BriefItemKind::Model => 3,
        BriefItemKind::Git => 4,
        BriefItemKind::Memory => 5,
    };
    items.sort_by_key(|i| order(i.kind));
    items
}

/// `Three days since you were last here.` after two days or more.
fn absence_line(away: Option<Duration>) -> Option<String> {
    let days = away?.as_secs() / 86_400;
    (days >= 2).then(|| format!("{} days since you were last here.", capitalize(&count_word(days))))
}

/// Small counts as words, the rest as digits.
fn count_word(n: u64) -> String {
    const WORDS: [&str; 11] = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];
    WORDS.get(n as usize).map(|w| w.to_string()).unwrap_or_else(|| n.to_string())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// `a`, `a and b`, `a, b and c`; semicolons when an item carries its own commas. More than
/// [`MAX_LISTED`] become "and N more".
fn join_list(texts: &[String]) -> String {
    let mut list: Vec<String> = texts.iter().take(MAX_LISTED).cloned().collect();
    if texts.len() > MAX_LISTED {
        list.push(format!("{} more", texts.len() - MAX_LISTED));
    }
    let sep = if list.iter().any(|t| t.contains(',')) { "; " } else { ", " };
    match list.len() {
        0 => String::new(),
        1 => list.remove(0),
        2 => format!("{} and {}", list[0], list[1]),
        n => format!("{}{sep}and {}", list[..n - 1].join(sep), list[n - 1]),
    }
}

/// `C:\Users\m\Projects\PruningResearch\valiance` -> `PruningResearch\valiance`: the part under
/// a `Projects` directory, else the last two components.
fn short_path(path: &str) -> String {
    let sep = if path.contains('\\') { "\\" } else { "/" };
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if let Some(i) = parts.iter().rposition(|p| p.eq_ignore_ascii_case("projects")) {
        if i + 1 < parts.len() {
            return parts[i + 1..].join(sep);
        }
    }
    parts[parts.len().saturating_sub(2)..].join(sep)
}

fn offer_sentence(offers: &[WatchCandidate]) -> Option<String> {
    let why = |c: &WatchCandidate| match c.live_runs {
        0 => "recent runs",
        1 => "a live run",
        _ => "live runs",
    };
    match offers {
        [] => None,
        [c] => Some(format!("I can watch {} — it has {}. Watch it?", short_path(&c.path), why(c))),
        [a, b, ..] => {
            let more = offers.len() - 2;
            let tail = if more > 0 { format!(", and {more} more") } else { String::new() };
            Some(format!(
                "I can watch {} ({}) and {} ({}){tail}. Watch them?",
                short_path(&a.path),
                why(a),
                short_path(&b.path),
                why(b)
            ))
        }
    }
}

/// Body sentences within `room`, chosen by priority and emitted in canonical order.
fn body(inp: &BriefInputs, part: DayPart, items: &[BriefItem], offer: Option<String>, room: usize) -> Vec<String> {
    let of = |k: BriefItemKind| items.iter().filter(|i| i.kind == k).map(|i| i.text.clone()).collect::<Vec<_>>();
    let (finished, attention, running) = (of(BriefItemKind::Finished), of(BriefItemKind::Attention), of(BriefItemKind::Running));
    let (models, git) = (of(BriefItemKind::Model), of(BriefItemKind::Git));
    // (priority, sentence) in canonical order.
    let mut out: Vec<(u8, String)> = Vec::new();
    if !finished.is_empty() {
        let lead = match inp.away {
            Some(a) if a >= OVERNIGHT && part == DayPart::Morning => "Overnight, ",
            Some(_) => "Since you were last here, ",
            None => "In the last day, ",
        };
        out.push((0, format!("{lead}{}.", join_list(&finished))));
    }
    if !attention.is_empty() {
        let head = match attention.len() {
            1 => "One thing needs you".to_string(),
            n => format!("{} things need you", capitalize(&count_word(n as u64))),
        };
        out.push((1, format!("{head}: {}.", join_list(&attention))));
    }
    match running.len() {
        0 => {}
        1 => out.push((2, format!("{} is still running.", running[0]))),
        _ => out.push((2, format!("Still running: {}.", join_list(&running)))),
    }
    if !models.is_empty() {
        out.push((4, format!("New in Ollama: {}.", join_list(&models))));
    }
    if !git.is_empty() {
        out.push((5, format!("{}.", capitalize(&join_list(&git)))));
    }
    if let Some(m) = &inp.memory {
        let degraded = if m.degraded.is_some() { "; recall is degraded" } else { "" };
        out.push((
            6,
            format!(
                "Memory holds {} and {}{degraded}.",
                plural(m.beliefs, "belief", "beliefs"),
                plural(m.episodes, "episode", "episodes")
            ),
        ));
    }
    if let Some(o) = offer {
        out.push((3, o));
    }
    // Fill the sentence budget by priority (the offer costs two), skipping what does not fit.
    let mut by_priority: Vec<(u8, usize)> = out.iter().map(|(p, s)| (*p, sentence_count(s))).collect();
    by_priority.sort_unstable();
    let mut left = room;
    let mut keep = Vec::new();
    for (p, cost) in by_priority {
        if cost <= left {
            left -= cost;
            keep.push(p);
        }
    }
    out.into_iter().filter(|(p, _)| keep.contains(p)).map(|(_, s)| s).collect()
}

/// Sentence ends: `.` or `?` followed by a space or the end (not a decimal point or `04:12`).
fn sentence_count(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    (0..chars.len()).filter(|&i| matches!(chars[i], '.' | '?') && chars.get(i + 1).is_none_or(|c| *c == ' ')).count()
}

// ---------------------------------------------------------------- spoken form

/// The ear-friendly version of a written text (no Markdown, last path component, no URLs,
/// numbers said the way a person says them).
pub fn spoken(text: &str) -> String {
    let mut words = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.trim_start_matches('#').trim_start_matches("> ");
        let line = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line);
        for w in unlink(line).split_whitespace() {
            words.push(spoken_word(w));
        }
    }
    let mut s = words.join(" ");
    for (from, to) in [
        ("**", ""),
        ("`", ""),
        ("→", " to "),
        ("—", ", "),
        ("–", ", "),
        ("·", ", "),
        ("%", " percent"),
        (" & ", " and "),
        ("(", ", "),
        (")", ", "),
        (";", ","),
        ("\"", ""),
        ("“", ""),
        ("”", ""),
    ] {
        s = s.replace(from, to);
    }
    tidy_punctuation(&s)
}

/// `[text](url)` -> `text`.
fn unlink(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let tail = &after[close + 2..];
        let Some(end) = tail.find(')') else { break };
        out.push_str(&rest[..open]);
        out.push_str(&after[..close]);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

fn spoken_word(w: &str) -> String {
    let lead_end = w.find(|c: char| !"(\"'“[".contains(c)).unwrap_or(w.len());
    let (lead, rest) = w.split_at(lead_end);
    let core_end = rest.trim_end_matches(|c: char| ".,;:)!?\"'”]".contains(c)).len();
    let (core, trail) = rest.split_at(core_end);
    let core = if core.starts_with("http://") || core.starts_with("https://") || core.starts_with("www.") {
        "a link".to_string()
    } else if is_decoration(core) {
        // `===` and friends are not words.
        String::new()
    } else if core.contains(['\\', '/']) && !core.chars().all(|c| c.is_ascii_digit() || c == '/') {
        if looks_like_path(core) {
            leaf(core).to_string()
        } else {
            // `NaN/inf`, `train/val`, `and/or`.
            core.replace('/', " or ")
        }
    } else if let Some(n) = say_number(core) {
        n
    } else {
        core.to_string()
    };
    format!("{lead}{}{trail}", core.replace('_', " "))
}

/// A real path: a backslash, a root (`/`, `~`, `./`, `C:`), or a last part with a file
/// extension. `NaN/inf` and `train/val` are not.
fn looks_like_path(core: &str) -> bool {
    let b = core.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if core.contains('\\') || core.starts_with(['/', '~']) || core.starts_with("./") || core.starts_with("../") || drive {
        return true;
    }
    leaf(core).rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && (1..=5).contains(&ext.len())
            && ext.starts_with(|c: char| c.is_ascii_alphabetic())
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// `1.21834` -> `1.22`; `12034` -> `about 12 thousand`; anything else -> `None` (unchanged).
fn say_number(core: &str) -> Option<String> {
    let digits = core.strip_prefix('-').unwrap_or(core);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') || digits.matches('.').count() > 1 {
        return None;
    }
    if digits.starts_with('.') || digits.ends_with('.') {
        return None;
    }
    let x: f64 = core.parse().ok()?;
    if core.contains('.') {
        return Some(round_sig(x, 3));
    }
    let a = x.abs();
    if a >= 1e6 {
        Some(format!("about {} million", round_sig(x / 1e6, 2)))
    } else if a >= 1e4 {
        Some(format!("about {} thousand", (x / 1e3).round()))
    } else {
        None
    }
}

/// `x` to `sig` significant figures, without trailing zeros.
fn round_sig(x: f64, sig: i32) -> String {
    if x == 0.0 || !x.is_finite() {
        return format!("{x}");
    }
    let mag = x.abs().log10().floor() as i32;
    let decimals = (sig - 1 - mag).max(0) as usize;
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Collapses the commas and spaces the symbol replacements leave behind.
fn tidy_punctuation(s: &str) -> String {
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let before = s.clone();
        for (from, to) in [(" ,", ","), (",,", ","), (", .", "."), (",.", "."), (", ?", "?"), (",?", "?"), (" .", "."), ("  ", " ")] {
            s = s.replace(from, to);
        }
        if s == before {
            break;
        }
    }
    s.trim().trim_start_matches(',').trim().to_string()
}

// ---------------------------------------------------------------- polish

/// The polish request: the briefing block's register over facts that are already decided.
pub(crate) fn polish_request(model: &str, system: Vec<SystemBlock>, inputs: &BriefInputs, brief: &Brief) -> ChatRequest {
    let away = match inputs.away {
        Some(a) => span(a.as_millis() as i64),
        None => "first session".into(),
    };
    let mut facts = String::new();
    for i in &brief.items {
        facts.push_str(&format!("- {}\n", i.text));
    }
    let prompt = format!(
        "Rewrite this opening briefing in your own voice. Use only the facts below and keep every name \
         and number exactly as written. Two to five sentences of plain prose: no Markdown, no lists. \
         If the draft says nothing happened, keep it to one line. Reply with the briefing only.\n\n\
         Local time: {}. Away: {away}.\n\nDraft:\n{}\n\nFacts:\n{facts}",
        inputs.now.format("%A %H:%M"),
        brief.text
    );
    let mut req = ChatRequest::new(model.to_string(), vec![Message::user(prompt)]);
    req.effort = Effort::Off;
    req.max_output_tokens = Some(POLISH_MAX_TOKENS);
    req.system = system;
    req
}

/// Whether the polish may call this provider now. A local model (Ollama, GGUF) loads onto the
/// GPU the user's own jobs are using, so it is skipped unless explicitly allowed, and even then
/// only while no watched run is live and every GPU is idle (an unknown GPU is not idle).
pub(crate) fn polish_allowed(kind: ProviderKind, allow_local: bool, runs: &[RunInfo], gpu: &GpuReport) -> bool {
    const IDLE_UTIL_PCT: u32 = 10;
    const IDLE_MEM_PCT: u64 = 25;
    if !matches!(kind, ProviderKind::Ollama | ProviderKind::LocalGguf) {
        return true;
    }
    let gpu_idle = !gpu.devices.is_empty()
        && gpu.devices.iter().all(|d| d.util_pct < IDLE_UTIL_PCT && d.mem_used_mb * 100 < d.mem_total_mb.max(1) * IDLE_MEM_PCT);
    allow_local && gpu_idle && !runs.iter().any(|r| r.status.is_live())
}

/// One polish call with a deadline. `None` on any failure, timeout, or a reply that fails
/// [`accept_polish`]; the caller keeps the deterministic text.
pub(crate) fn polish(provider: Arc<dyn Provider>, req: ChatRequest, draft: &str, attention: &[String], timeout: Duration) -> Option<String> {
    let cancel = CancelToken::new();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let token = cancel.clone();
    std::thread::Builder::new()
        .name("marlowe-brief-polish".into())
        .spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| provider.stream(&req, &token, &mut |_| {})));
            let _ = tx.send(r.ok().and_then(Result::ok).map(|c| c.text));
        })
        .ok()?;
    let text = match rx.recv_timeout(timeout) {
        Ok(Some(t)) => t,
        _ => {
            // Detached: the call ends at the provider's next cancel check.
            cancel.cancel();
            return None;
        }
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches('"').trim().to_string();
    accept_polish(draft, attention, &text).then_some(text)
}

/// Number-like tokens: `1.218`, `04:12`, the `5` in `wnr5`.
fn numbers(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || ((chars[i] == '.' || chars[i] == ':' || chars[i] == ',') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())))
            {
                i += 1;
            }
            out.insert(chars[start..i].iter().collect());
        } else {
            i += 1;
        }
    }
    out
}

/// Phrases the polish may not use: persona v3's `<words_to_avoid>` (the comma lists under each
/// `- ` bullet, after any `label:`), plus their common short forms.
fn banned_phrases() -> &'static [String] {
    static BANNED: OnceLock<Vec<String>> = OnceLock::new();
    BANNED.get_or_init(|| {
        let core = marlowe_agent::PERSONA_CORE;
        let section = core
            .split_once("<words_to_avoid>")
            .and_then(|(_, rest)| rest.split_once("</words_to_avoid>"))
            .map_or("", |(s, _)| s);
        let mut out: Vec<String> = section
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- "))
            .map(|l| l.split_once(':').map_or(l, |(_, rest)| rest))
            .flat_map(|l| l.split(','))
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        for extra in ["i hope", "happy to", "let me know", "feel free", "it's worth noting", "great question"] {
            if !out.iter().any(|w| w == extra) {
                out.push(extra.to_string());
            }
        }
        out
    })
}

/// Emoji and pictographs (persona v3: no emoji in any channel).
fn is_pictograph(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F)
}

/// A polished brief is used only if it is plain prose of two to five sentences at a sane length,
/// invents no number, keeps every attention item the draft states (its name and its numbers),
/// and avoids the persona's banned register and emoji. `attention` is the brief's Attention
/// item texts; those the draft does not state are not required.
pub(crate) fn accept_polish(draft: &str, attention: &[String], polished: &str) -> bool {
    let p = polished.trim();
    if p.is_empty() || p.chars().count() > (draft.chars().count() * 2).max(600) || p.contains('!') {
        return false;
    }
    if p.contains("```") || p.contains("**") || p.starts_with('#') || p.starts_with("- ") || p.starts_with("* ") {
        return false;
    }
    if p.chars().any(is_pictograph) || sentence_count(p) > MAX_SENTENCES {
        return false;
    }
    let lower = p.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    let draft_lower = draft.to_lowercase();
    // A banned word inside a fact (a run named `robust-ft`) is the fact, not the register.
    if banned_phrases().iter().any(|b| lower.contains(b.as_str()) && !draft_lower.contains(b.as_str())) {
        return false;
    }
    let known = numbers(draft);
    if !numbers(p).iter().all(|n| known.contains(n)) {
        return false;
    }
    let said = numbers(p);
    attention.iter().filter(|a| draft.contains(a.as_str())).all(|a| {
        let name = a.split_whitespace().next().unwrap_or("").trim_end_matches([':', ',', ';']);
        p.contains(name) && numbers(a).iter().all(|n| said.contains(n))
    })
}

// ---------------------------------------------------------------- daemon hooks

/// Called after `Welcome` for each client. Briefs `client` when [`should_brief`] holds; never
/// blocks the reader (the work runs on its own thread).
pub(crate) fn on_hello(core: &Arc<Core>, client: u64, client_name: &str) {
    crate::brief_ops::on_hello(core, client, client_name);
}

/// Called when a client disconnects: records presence for the away rule.
pub(crate) fn on_goodbye(core: &Arc<Core>, client_name: &str) {
    crate::brief_ops::on_goodbye(core, client_name);
}

/// Answers `Request::Brief`: composes now, regardless of absence, and replies to `client` only.
pub(crate) fn reply(core: &Arc<Core>, client: u64) {
    crate::brief_ops::reply(core, client);
}

/// Speaks an urgent Sentinel notice when voice is on and it is outside quiet hours.
pub(crate) fn speak_notice(core: &Arc<Core>, notice: &Notice) {
    crate::brief_ops::speak_notice(core, notice);
}

#[cfg(test)]
#[path = "brief_tests.rs"]
mod tests;
