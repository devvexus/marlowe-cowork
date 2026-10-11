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
pub use marlowe_sentinel::metrics::ResultComparison;

use crate::daemon::Core;

#[path = "brief_words.rs"]
mod words;

pub(crate) use crate::brief_ops::Presence;

/// Away at least this long before a brief is offered.
pub const AWAY_THRESHOLD: Duration = Duration::from_secs(2 * 3600);

/// The optional LLM polish gives up after this long.
pub const POLISH_TIMEOUT: Duration = Duration::from_secs(6);

/// A brief is at most this many sentences, greeting included.
const MAX_SENTENCES: usize = 5;

/// Items listed per sentence before "and N more".
const MAX_LISTED: usize = 3;

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
    /// Memory health; only a degraded recall is said.
    pub memory: Option<MemoryStatsReport>,
    /// Profile memories recorded since the last brief; said only when non-zero.
    pub memory_news: MemoryNews,
    /// Watch roots, for naming runs the way the person does.
    pub roots: Vec<BriefRoot>,
    /// Result files changed since last seen, beside their previous siblings, newest first.
    pub results: Vec<ResultComparison>,
    /// Discovery candidates to offer.
    pub offers: Vec<WatchCandidate>,
    /// The Sentinel answered. False when it is not running (or failed to report): the brief
    /// then says it knows nothing about runs rather than that nothing moved.
    pub sentinel: bool,
}

/// A watch root and the heartbeat pipelines under it: a root with one is named for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefRoot {
    pub path: String,
    pub pipelines: u32,
}

/// Profile memories new since the last brief.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryNews {
    /// At the user's own trust: things they said.
    pub from_user: u32,
    pub other: u32,
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

/// A heartbeat-driven pipeline rather than a directory of logs.
pub(crate) fn is_pipeline(r: &RunInfo) -> bool {
    words::is_pipeline(r)
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

/// A brief is at most this many words, greeting included.
const MAX_WORDS: usize = 90;

/// A result's sibling written further apart than this is a stale comparison.
const FRESH_SIBLING_MS: i64 = 24 * 3600 * 1000;

/// Body priorities: lower is kept first when sentences or words run short.
const P_LEAD: u8 = 0;
const P_ATTENTION: u8 = 1;
const P_RESULT: u8 = 2;
const P_OTHERS: u8 = 3;
const P_RUNNING: u8 = 4;
const P_OFFER: u8 = 5;
const P_MODELS: u8 = 6;
const P_GIT: u8 = 7;
const P_MEMORY: u8 = 8;

/// One body sentence, with a spoken form when the ear needs other words (a folder, not a path).
struct Line {
    priority: u8,
    text: String,
    spoken: Option<String>,
}

impl Line {
    fn new(priority: u8, text: String) -> Line {
        Line { priority, text, spoken: None }
    }
}

/// Renders the deterministic brief.
pub fn compose(inputs: &BriefInputs) -> Brief {
    let (mut lines, items) = story(inputs);
    let part = DayPart::of(&inputs.now);
    let mut head = vec![greeting(part, inputs.address.as_deref())];
    if let Some(a) = absence_line(inputs.away) {
        head.push(a);
    }
    if items.iter().all(|i| i.kind == BriefItemKind::Memory) {
        let line = if !inputs.sentinel {
            "The Sentinel is not running, so I have nothing on your runs."
        } else if inputs.away.is_none() {
            "Nothing to report yet."
        } else {
            "Nothing moved while you were away."
        };
        lines.insert(0, Line::new(P_LEAD, line.into()));
    }
    let words = head.iter().map(|h| word_count(h)).sum::<usize>();
    let body = fit(lines, MAX_SENTENCES.saturating_sub(head.len()), MAX_WORDS.saturating_sub(words));
    let said: Vec<String> = head.iter().cloned().chain(body.iter().map(|l| l.spoken.clone().unwrap_or_else(|| l.text.clone()))).collect();
    let text = head.into_iter().chain(body.into_iter().map(|l| l.text)).collect::<Vec<_>>().join(" ");
    Brief { spoken: spoken(&said.join(" ")), text, items, offers: inputs.offers.clone() }
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count()
}

/// Keeps lines by priority within `room` sentences and `words` words, in their given order.
fn fit(lines: Vec<Line>, room: usize, words: usize) -> Vec<Line> {
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by_key(|&i| lines[i].priority);
    let (mut s_left, mut w_left) = (room, words);
    let mut keep = vec![false; lines.len()];
    for i in order {
        let (s, w) = (sentence_count(&lines[i].text).max(1), word_count(&lines[i].text));
        if s <= s_left && w <= w_left {
            keep[i] = true;
            s_left -= s;
            w_left -= w;
        }
    }
    lines.into_iter().zip(keep).filter_map(|(l, k)| k.then_some(l)).collect()
}

/// `HH:MM` local for an epoch-ms instant.
fn clock(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%H:%M").to_string(),
        None => "?".into(),
    }
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
        parts.push(format!("loss {}", round_sig(l, 4)));
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

/// The innermost watch root holding `dir`.
fn root_of<'a>(dir: &str, roots: &'a [BriefRoot]) -> Option<&'a BriefRoot> {
    roots.iter().filter(|r| within(dir, &r.path)).max_by_key(|r| r.path.len())
}

/// What the person calls a run: the root's name when the root holds one pipeline (`Valiance`),
/// else the run name without the project prefix (`wnr-final`); a log directory by its name.
fn display_name(r: &RunInfo, roots: &[BriefRoot]) -> String {
    if !words::is_pipeline(r) {
        return r.name.clone();
    }
    match root_of(&r.dir, roots) {
        Some(root) if root.pipelines <= 1 => capitalize(leaf(&root.path)),
        Some(root) => words::shorten(&r.name, leaf(&root.path)),
        None => r.name.clone(),
    }
}

/// Capitalises a sentence that opens with a plain lowercase word (`coherence`), not with an
/// identifier (`wnr7`, `ft-a`).
fn sentence_case(s: &str) -> String {
    let first = s.split_whitespace().next().unwrap_or("");
    if !first.is_empty() && first.chars().all(|c| c.is_ascii_lowercase()) {
        capitalize(s)
    } else {
        s.to_string()
    }
}

/// A finished or failed run that only a notice still remembers (the Sentinel has dropped it).
fn run_from_notice(n: &Notice, runs: &[RunInfo]) -> Option<RunInfo> {
    let status = match (n.source, n.kind.as_str()) {
        (NoticeSource::Run | NoticeSource::Trainer, "finished") => RunStatus::Finished,
        (NoticeSource::Run, "failed") => RunStatus::Failed,
        (NoticeSource::Run, "ended") => RunStatus::Ended,
        _ => return None,
    };
    if runs.iter().any(|r| within(&n.subject, &r.dir) || within(&r.dir, &n.subject)) {
        return None;
    }
    let title = n.title.trim();
    let name = [" finished training", " finished", " failed", " ended"]
        .iter()
        .find_map(|s| title.split_once(s).map(|(a, _)| a.trim()))
        .filter(|s| !s.is_empty())
        .unwrap_or(leaf(&n.subject));
    // A run notice's detail ends with the run's last line.
    let last_line = (n.source == NoticeSource::Run)
        .then(|| n.detail.rsplit(" · ").next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty());
    Some(RunInfo {
        dir: n.subject.clone(),
        name: name.to_string(),
        status,
        stage: None,
        layer: None,
        pid: None,
        heartbeat_ms: None,
        last_line,
        step: None,
        loss: None,
        since_ms: n.at_ms,
    })
}

/// `HH:MM` a notice happened: from its title's ` at HH:MM[:SS]`, else when it was raised.
fn notice_clock(n: &Notice) -> String {
    n.title.rsplit_once(" at ").map(|(_, t)| t.trim().chars().take(5).collect::<String>()).filter(|t| t.len() == 5 && t.as_bytes()[2] == b':').unwrap_or_else(|| clock(n.at_ms))
}

/// A notice detail clean and short enough to say; noisy lines stay on the board.
fn clean_detail(d: &str) -> Option<String> {
    let body = log_line_body(d);
    (!body.is_empty() && !words::is_noisy(&body)).then_some(body)
}

/// `coherence finished and longhorizon stopped yesterday`: the runs after the lead, grouped by
/// how and when they ended.
fn others_text(others: &[(String, words::Outcome, i64)], now: &DateTime<Local>) -> String {
    let day_of = |ms: i64| words::day_word(ms, now);
    // (verb, day, names, failure reasons, times)
    type Group = (&'static str, Option<String>, Vec<String>, Vec<Option<String>>, Vec<i64>);
    let mut groups: Vec<Group> = Vec::new();
    for verb in ["finished", "stopped", "failed"] {
        for (name, o, at) in others.iter().filter(|(_, o, _)| o.verb() == verb) {
            let day = day_of(*at);
            let why = match o {
                words::Outcome::Failed(w) => w.clone(),
                _ => None,
            };
            match groups.iter_mut().find(|g| g.0 == verb && g.1 == day) {
                Some(g) => {
                    g.2.push(name.clone());
                    g.3.push(why);
                    g.4.push(*at);
                }
                None => groups.push((verb, day, vec![name.clone()], vec![why], vec![*at])),
            }
        }
    }
    // One day for several clauses is said once, at the end.
    let shared_day = groups
        .first()
        .and_then(|g| g.1.clone())
        .filter(|d| groups.len() > 1 && groups.iter().all(|g| g.1.as_deref() == Some(d)));
    let clauses: Vec<String> = groups
        .iter()
        .map(|(verb, day, names, whys, ats)| {
            let when = match (&shared_day, day) {
                (Some(_), _) => String::new(),
                (None, Some(d)) => format!(" {d}"),
                (None, None) if ats.len() == 1 => format!(" {}", words::when(ats[0], now)),
                (None, None) => " earlier today".into(),
            };
            let labelled = |i: usize| match &whys[i] {
                Some(w) => format!("{} ({w})", names[i]),
                None => names[i].clone(),
            };
            match (names.len(), &whys[0]) {
                // A lone failure gives its reason after the day: `failed yesterday (exit code 1)`.
                (1, Some(w)) if shared_day.is_none() => format!("{} {verb}{when} ({w})", names[0]),
                _ => format!("{} {verb}{when}", join_list(&(0..names.len()).map(labelled).collect::<Vec<_>>())),
            }
        })
        .collect();
    let mut text = match clauses.len() {
        0 => String::new(),
        1 => clauses[0].clone(),
        2 if !clauses[0].contains(" and ") => format!("{} and {}", clauses[0], clauses[1]),
        n => format!("{}, and {}", clauses[..n - 1].join(", "), clauses[n - 1]),
    };
    if let Some(d) = shared_day {
        text.push_str(&format!(" {d}"));
    }
    text
}

fn needs(texts: &[String]) -> String {
    let head = match texts.len() {
        1 => "One thing needs you".to_string(),
        n => format!("{} things need you", capitalize(&count_word(n as u64))),
    };
    format!("{head}: {}.", join_list(texts))
}

/// The body lines in canonical order (what finished and how it measured, what needs you, what
/// is still running, models, git, memory, the offer) and the structured items behind them.
fn story(inp: &BriefInputs) -> (Vec<Line>, Vec<BriefItem>) {
    let now = &inp.now;
    let now_ms = now.timestamp_millis();
    let mut lines = Vec::new();
    let mut items = Vec::new();
    let item = |kind, severity, text: &str, subject: Option<&str>| BriefItem {
        kind,
        severity,
        text: text.to_string(),
        subject: subject.map(String::from),
    };

    let mut notices: Vec<&Notice> = inp.notices.iter().collect();
    // Alarms first; stable otherwise (the caller ranked them).
    notices.sort_by_key(|n| std::cmp::Reverse(n.severity));
    let mut runs = inp.runs.clone();
    for n in &notices {
        if let Some(r) = run_from_notice(n, &runs) {
            runs.push(r);
        }
    }

    // Notices the runs do not already tell: alarms outside a failed run, stage endings inside a
    // live one, models and commits.
    let mut seen_lines: BTreeSet<(String, String)> = BTreeSet::new();
    let mut stage_done: Vec<(String, String)> = Vec::new();
    let mut attention: Vec<String> = Vec::new();
    let mut models: Vec<String> = Vec::new();
    let mut git: Vec<String> = Vec::new();
    for n in &notices {
        // Two logs in one run dir often carry the same line (a stage log and its tee).
        if n.source == NoticeSource::Log && !n.detail.trim().is_empty() {
            let dir = n.subject.rsplit_once(['\\', '/']).map_or("", |(d, _)| d).to_string();
            if !seen_lines.insert((dir, log_line_body(&n.detail))) {
                continue;
            }
        }
        let run = runs.iter().find(|r| within(&n.subject, &r.dir));
        match n.source {
            NoticeSource::Ollama => {
                if !models.contains(&n.subject) {
                    models.push(n.subject.clone());
                }
                continue;
            }
            NoticeSource::Git => {
                git.push(n.title.trim().to_string());
                continue;
            }
            NoticeSource::Log if n.severity == Severity::Good => {
                // A log's completion inside a live run is a stage ending, not the run.
                if let Some(r) = run.filter(|r| r.status.is_live()) {
                    if let Some(body) = clean_detail(&n.detail) {
                        if !stage_done.iter().any(|(d, _)| *d == r.dir) {
                            stage_done.push((r.dir.clone(), format!("{body} at {}", notice_clock(n))));
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        if !n.severity.is_urgent() {
            continue;
        }
        match run {
            // The run's state says it: a stall it is still in, a failure, or one it moved on from.
            Some(_) if n.source == NoticeSource::Run => continue,
            Some(r) if !r.status.is_live() && matches!(words::outcome(r), words::Outcome::Failed(_)) => continue,
            _ => {}
        }
        let mut t = n.title.trim().to_string();
        if let Some(d) = clean_detail(&n.detail) {
            t.push_str(&format!(" ({d})"));
        }
        items.push(item(BriefItemKind::Attention, n.severity, &t, Some(&n.subject)));
        attention.push(t);
    }

    // Runs that ended since you were here: the most prominent leads (a pipeline over a log
    // directory, then the latest), the rest follow in one sentence.
    let mut ended: Vec<&RunInfo> = runs.iter().filter(|r| !r.status.is_live()).collect();
    ended.sort_by(|a, b| words::is_pipeline(b).cmp(&words::is_pipeline(a)).then(b.since_ms.cmp(&a.since_ms)));
    let lead = ended.first().copied();
    let outcome_item = |r: &RunInfo, o: &words::Outcome, text: &str| {
        let (kind, sev) = match o {
            words::Outcome::Finished => (BriefItemKind::Finished, Severity::Good),
            words::Outcome::Failed(_) => (BriefItemKind::Attention, Severity::Attention),
            words::Outcome::Stopped => (BriefItemKind::Finished, Severity::Info),
        };
        item(kind, sev, text, Some(&r.dir))
    };
    if let Some(r) = lead {
        let o = words::outcome(r);
        let root_leaf = root_of(&r.dir, &inp.roots).map(|x| leaf(&x.path));
        let short = root_leaf.map_or_else(|| r.name.clone(), |l| words::shorten(&r.name, l));
        let detail = match &o {
            words::Outcome::Finished => r.last_line.as_deref().and_then(|l| words::paraphrase(l, &r.name, &short, root_leaf)),
            words::Outcome::Failed(why) => why.clone(),
            words::Outcome::Stopped => None,
        };
        let mut text = format!("{} {} {}", display_name(r, &inp.roots), o.verb(), words::when(r.since_ms, now));
        if let Some(d) = detail {
            text.push_str(&format!(" — {d}"));
        }
        let text = sentence_case(&text);
        items.push(outcome_item(r, &o, &text));
        lines.push(Line::new(P_LEAD, format!("{text}.")));
    }

    // The newest result judged against its previous sibling, preferring the lead's and one whose
    // sibling is fresh (a two-day-old match compares eras, not steps); "its" when it is the lead's.
    let lead_root = lead.and_then(|r| root_of(&r.dir, &inp.roots));
    let ours = |path: &str| lead.is_some_and(|r| lead_root.map_or(within(path, &r.dir), |root| within(path, &root.path)));
    let fresh = |c: &&ResultComparison| (c.modified_ms - c.sibling_modified_ms).abs() <= FRESH_SIBLING_MS;
    let result = [(true, true), (true, false), (false, true), (false, false)]
        .into_iter()
        .find_map(|(need_ours, need_fresh)| inp.results.iter().find(|c| (!need_ours || ours(&c.path)) && (!need_fresh || fresh(c))));
    if let Some(c) = result {
        if let Some((t, beat)) = words::result_sentence(c, if ours(&c.path) { "Its" } else { "The latest" }) {
            items.push(item(BriefItemKind::Finished, if beat { Severity::Good } else { Severity::Info }, &t, Some(&c.path)));
            lines.push(Line::new(P_RESULT, format!("{t}.")));
        }
    }

    let others: Vec<(String, words::Outcome, i64)> =
        ended.iter().skip(1).map(|r| (display_name(r, &inp.roots), words::outcome(r), r.since_ms)).collect();
    for ((name, o, at), r) in others.iter().zip(ended.iter().skip(1)) {
        let why = match o {
            words::Outcome::Failed(Some(w)) => format!(" ({w})"),
            _ => String::new(),
        };
        items.push(outcome_item(r, o, &format!("{name} {} {}{why}", o.verb(), words::when(*at, now))));
    }
    if !others.is_empty() {
        lines.push(Line::new(P_OTHERS, format!("{}.", sentence_case(&others_text(&others, now)))));
    }

    // Live runs: a stall needs you; the rest are still running.
    let mut running = Vec::new();
    let mut stalls = Vec::new();
    for r in runs.iter().filter(|r| r.status.is_live()) {
        let name = display_name(r, &inp.roots);
        if r.status == RunStatus::Stalled {
            let at = r.layer.map(|l| format!(" at layer {l}")).unwrap_or_default();
            let quiet = r.heartbeat_ms.map(|h| format!(" (no heartbeat for {})", span(now_ms - h))).unwrap_or_default();
            let t = format!("{name} has stalled{at}{quiet}");
            items.push(item(BriefItemKind::Attention, Severity::Attention, &t, Some(&r.dir)));
            stalls.push(t);
            continue;
        }
        let done = stage_done.iter().find(|(d, _)| *d == r.dir).map(|(_, s)| s.as_str());
        let t = match (run_progress(r), done) {
            (Some(p), Some(d)) => format!("{name} ({p}; {d})"),
            (Some(p), None) => format!("{name} ({p})"),
            (None, Some(d)) => format!("{name} ({d})"),
            (None, None) => name,
        };
        items.push(item(BriefItemKind::Running, Severity::Info, &t, Some(&r.dir)));
        running.push(t);
    }
    if !attention.is_empty() || !stalls.is_empty() {
        stalls.append(&mut attention);
        lines.push(Line::new(P_ATTENTION, needs(&stalls)));
    }
    match running.len() {
        0 => {}
        1 => lines.push(Line::new(P_RUNNING, format!("{} is still running.", sentence_case(&running[0])))),
        _ => lines.push(Line::new(P_RUNNING, format!("Still running: {}.", join_list(&running)))),
    }

    for m in &inp.new_models {
        if !models.contains(m) {
            models.push(m.clone());
        }
    }
    for m in &models {
        items.push(item(BriefItemKind::Model, Severity::Info, m, Some(m)));
    }
    if !models.is_empty() {
        lines.push(Line::new(P_MODELS, format!("New in Ollama: {}.", join_list(&models))));
    }

    for (ws, g) in &inp.git {
        let noticed = inp.notices.iter().any(|n| n.source == NoticeSource::Git && (n.subject == *ws || within(ws, &n.subject)));
        if g.new_commits > 0 && !noticed {
            git.push(git_text(ws, g));
        }
    }
    for t in &git {
        items.push(item(BriefItemKind::Git, Severity::Info, t, None));
    }
    if !git.is_empty() {
        lines.push(Line::new(P_GIT, format!("{}.", capitalize(&join_list(&git)))));
    }

    if let Some(t) = memory_news(&inp.memory_news) {
        items.push(item(BriefItemKind::Memory, Severity::Info, &t, None));
        lines.push(Line::new(P_MEMORY, format!("{t}.")));
    }
    if inp.memory.as_ref().is_some_and(|m| m.degraded.is_some()) {
        let t = "Memory recall is degraded";
        items.push(item(BriefItemKind::Memory, Severity::Attention, t, None));
        lines.push(Line::new(P_MEMORY, format!("{t}.")));
    }
    if let Some((text, said)) = offer_sentence(&inp.offers) {
        lines.push(Line { priority: P_OFFER, text, spoken: Some(said) });
    }
    (lines, items)
}

/// `I recorded three new things you told me`; nothing when nothing changed.
fn memory_news(m: &MemoryNews) -> Option<String> {
    let things = |n: u32| if n == 1 { "thing" } else { "things" };
    let (u, o) = (m.from_user, m.other);
    match (u, o) {
        (0, 0) => None,
        (u, 0) => Some(format!("I recorded {} new {} you told me", count_word(u as u64), things(u))),
        (0, o) => Some(format!("I noted {} new {} about your work", count_word(o as u64), things(o))),
        (u, o) => Some(format!("I recorded {} new things, {} of them from you", count_word((u + o) as u64), count_word(u as u64))),
    }
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

/// One short question, written with the folder's path and said with its name:
/// `PruningResearch\engine has fresh runs — shall I watch it?` /
/// `The engine folder has fresh runs — shall I watch it?`.
fn offer_sentence(offers: &[WatchCandidate]) -> Option<(String, String)> {
    let why = |c: &WatchCandidate| match c.live_runs {
        0 => "fresh runs",
        1 => "a live run",
        _ => "live runs",
    };
    let folder = |c: &WatchCandidate| format!("the {} folder", leaf(&c.path));
    match offers {
        [] => None,
        [c] => Some((
            format!("{} has {} — shall I watch it?", short_path(&c.path), why(c)),
            capitalize(&format!("{} has {} — shall I watch it?", folder(c), why(c))),
        )),
        [a, b, rest @ ..] => {
            let more = if rest.is_empty() { String::new() } else { format!(" (and {} more)", rest.len()) };
            let say = |x: String, y: String| {
                if why(a) == why(b) {
                    format!("{x} and {y} have {}{more} — shall I watch them?", why(a))
                } else {
                    format!("{x} has {} and {y} has {}{more} — shall I watch them?", why(a), why(b))
                }
            };
            Some((say(short_path(&a.path), short_path(&b.path)), capitalize(&say(folder(a), folder(b)))))
        }
    }
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

/// Whether the polish may call this provider now. The model is already loaded (the caller never
/// picks one that would load), but a local one shares the GPU with the user's own work, so it is
/// skipped unless explicitly allowed, and even then only while no turn or crew run is using it,
/// no watched run is live and every GPU is idle (an unknown GPU is not idle).
pub(crate) fn polish_allowed(kind: ProviderKind, allow_local: bool, foreground_busy: bool, runs: &[RunInfo], gpu: &GpuReport) -> bool {
    const IDLE_UTIL_PCT: u32 = 10;
    if !matches!(kind, ProviderKind::Ollama | ProviderKind::LocalGguf) {
        return true;
    }
    let gpu_idle = !gpu.devices.is_empty() && gpu.devices.iter().all(|d| d.util_pct < IDLE_UTIL_PCT);
    allow_local && !foreground_busy && gpu_idle && !runs.iter().any(|r| r.status.is_live())
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
