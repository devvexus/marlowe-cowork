//! The brief's daemon side: presence (the away rule), gathering inputs from the Sentinel, the
//! store and the Mind, the optional polish, storing the brief in the day's session, delivery, and
//! speaking (the brief, and urgent notices outside quiet hours).

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, NaiveDate, TimeZone, Timelike};
use marlowe_agent::{PersonaBlock, SystemPrompt};
use marlowe_model::Message;
use marlowe_proto::{
    BriefItemKind, Event, GpuReport, Notice, NoticeSource, QuietHours, SessionId, SessionSummary, Severity, Slot, WatchCandidate,
};

use crate::brief::{self, Brief, BriefInputs, BriefRoot, MemoryNews, POLISH_TIMEOUT};
use crate::daemon::Core;
use crate::{lock, now_ms};

/// Client names that mean the user is at the keyboard. Headless commands and probes are not.
const PRESENCE_CLIENTS: [&str; 1] = ["tui"];

/// `cowork.db` daemon_kv keys.
const LAST_SEEN: &str = "presence.last_seen_ms";
const DAY_SESSION: &str = "brief.day_session";
const OFFERED: &str = "brief.offered:";
/// Profile memory ids as of the last delivered brief, newline-separated.
const PROFILE_SEEN: &str = "brief.profile_ids";

/// A discovery candidate is offered again after this long.
const OFFER_AGAIN_MS: i64 = 3 * 86_400_000;

/// Most discovery offers per brief.
const MAX_OFFERS: usize = 2;

/// Notices window when there is no last-seen time.
const NO_HISTORY_WINDOW_MS: i64 = 86_400_000;

/// Right after the daemon starts, the Sentinel's first scan gets this long before a brief reads it.
const SENTINEL_SETTLE: Duration = Duration::from_secs(3);

/// Sessions updated within this window name the active workspaces.
const ACTIVE_WORKSPACE_MS: i64 = 7 * 86_400_000;
const MAX_GIT_WORKSPACES: usize = 3;

/// Urgent notices raised in the first moments after start are the brief's to tell, not the voice's.
const NOTICE_SPEECH_AFTER_START: Duration = Duration::from_secs(30);

/// At most one spoken notice per this long.
const NOTICE_SPEECH_GAP_MS: i64 = 20_000;

/// Spoken notice ids remembered; past this the memory starts over.
const MAX_SPOKEN_IDS: usize = 4096;

/// Interactive clients present now, when a notice was last spoken, and which notices were spoken
/// at what severity (a notice is said once, and again only when it escalates).
#[derive(Default)]
pub(crate) struct Presence {
    here: Mutex<usize>,
    last_spoken_ms: Mutex<i64>,
    spoken: Mutex<HashMap<String, Severity>>,
}

impl Presence {
    /// Whether a notice may be spoken now, recording it when so: said once per id, again only on
    /// a severity escalation (a re-raise with a new detail is for the screen), and never within
    /// [`NOTICE_SPEECH_GAP_MS`] of the last one.
    fn take_turn(&self, n: &Notice, now: i64) -> bool {
        let mut spoken = lock(&self.spoken);
        if spoken.get(&n.id).is_some_and(|s| *s >= n.severity) {
            return false;
        }
        let mut last = lock(&self.last_spoken_ms);
        if now - *last < NOTICE_SPEECH_GAP_MS {
            return false;
        }
        *last = now;
        if spoken.len() >= MAX_SPOKEN_IDS {
            spoken.clear();
        }
        spoken.insert(n.id.clone(), n.severity);
        true
    }
}

fn is_presence(name: &str) -> bool {
    PRESENCE_CLIENTS.contains(&name)
}

/// Runs `f`, turning a panic into `None`: a broken watcher must not take a client thread down.
fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

/// The latest of the last presence mark and the last saved turn.
fn last_seen(core: &Core) -> Option<i64> {
    let mark = core.store.kv(LAST_SEEN).ok().flatten().and_then(|v| v.parse::<i64>().ok());
    let activity = core.store.last_activity().ok().flatten();
    mark.max(activity)
}

fn local_date(ms: i64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|t| t.date_naive())
}

pub(crate) fn on_hello(core: &Arc<Core>, client: u64, client_name: &str) {
    if !is_presence(client_name) {
        return;
    }
    // Someone is here: a voice that loads a model (the Chatterbox clone) starts loading now, so
    // the greeting does not wait for a cold start. Never blocks; WinRT ignores it.
    if let Some(v) = &core.voice {
        if !quiet_hours(core).contains(local_minute()) {
            v.prewarm();
        }
    }
    // Decided under the lock so two interactive clients arriving together brief at most once.
    let decision = {
        let mut here = lock(&core.presence.here);
        *here += 1;
        if *here > 1 {
            None
        } else {
            let last = last_seen(core);
            let _ = core.store.set_kv(LAST_SEEN, &now_ms().to_string());
            Some(last)
        }
    };
    let Some(last) = decision else { return };
    let now = now_ms();
    let away = last.map(|l| Duration::from_millis((now - l).max(0) as u64));
    let first_today = last.and_then(local_date) != Some(Local::now().date_naive());
    if !brief::should_brief(away, first_today) {
        return;
    }
    let core = core.clone();
    let _ = std::thread::Builder::new().name("marlowe-brief".into()).spawn(move || {
        let Some((inputs, b, profile)) = guard(|| build(&core, last, away, first_today)) else {
            return core.bus.reply(client, Event::Error { session: None, message: "the brief failed".into() });
        };
        let session = match store_brief(&core, &b.text) {
            Ok(id) => Some(id),
            Err(e) => {
                core.bus.reply(client, Event::Error { session: None, message: format!("brief not saved: {e}") });
                None
            }
        };
        mark_offered(&core, &inputs.offers);
        if let Some(ids) = profile {
            let _ = core.store.set_kv(PROFILE_SEEN, &ids.join("\n"));
        }
        let said = b.spoken.clone();
        core.bus.reply(client, Event::Brief { text: b.text, spoken: b.spoken, items: b.items, offers: b.offers, session });
        speak(&core, &said);
    });
}

pub(crate) fn on_goodbye(core: &Arc<Core>, client_name: &str) {
    if !is_presence(client_name) {
        return;
    }
    let mut here = lock(&core.presence.here);
    *here = here.saturating_sub(1);
    let _ = core.store.set_kv(LAST_SEEN, &now_ms().to_string());
}

/// `Request::Brief`: composed now over what changed since the user was last seen; not stored,
/// not spoken, offers not marked.
pub(crate) fn reply(core: &Arc<Core>, client: u64) {
    let core = core.clone();
    let _ = std::thread::Builder::new().name("marlowe-brief".into()).spawn(move || {
        let last = last_seen(&core);
        let away = last.map(|l| Duration::from_millis((now_ms() - l).max(0) as u64));
        match guard(|| build(&core, last, away, false)) {
            Some((_, b, _)) => core.bus.reply(
                client,
                Event::Brief { text: b.text, spoken: b.spoken, items: b.items, offers: b.offers, session: None },
            ),
            None => core.bus.reply(client, Event::Error { session: None, message: "the brief failed".into() }),
        }
    });
}

/// Gathers inputs, composes, and polishes when there is something to say.
fn build(core: &Core, last_seen: Option<i64>, away: Option<Duration>, first_today: bool) -> (BriefInputs, Brief, Option<Vec<String>>) {
    let now = Local::now();
    let now_ms = now.timestamp_millis();
    let since = last_seen.unwrap_or(now_ms - NO_HISTORY_WINDOW_MS);
    let mut inputs = BriefInputs {
        now,
        away,
        first_today,
        address: core.user().address,
        notices: Vec::new(),
        runs: Vec::new(),
        new_models: Vec::new(),
        git: Vec::new(),
        memory: None,
        memory_news: MemoryNews::default(),
        roots: Vec::new(),
        results: Vec::new(),
        offers: Vec::new(),
        sentinel: false,
    };
    let mut gpu = GpuReport::default();
    if let Some(s) = &core.sentinel {
        if let Some(wait) = SENTINEL_SETTLE.checked_sub(core.uptime()) {
            std::thread::sleep(wait);
        }
        inputs.notices = guard(|| s.notices_since(since)).and_then(Result::ok).unwrap_or_default();
        inputs.notices.retain(|n| n.source != NoticeSource::Discovery);
        let state = guard(|| s.state());
        inputs.sentinel = state.is_some();
        let state = state.unwrap_or_default();
        inputs.roots = state
            .roots
            .iter()
            .map(|root| {
                let under = |d: &str| d == root.path || Path::new(d).starts_with(&root.path);
                let pipelines = state.runs.iter().filter(|r| under(&r.dir) && brief::is_pipeline(r)).count() as u32;
                BriefRoot { path: root.path.clone(), pipelines }
            })
            .collect();
        inputs.results = results_since(s, &inputs.notices);
        inputs.runs = state.runs.into_iter().filter(|r| r.status.is_live() || r.since_ms >= since).collect();
        inputs.new_models = state.ollama.new_models;
        gpu = state.gpu;
        for ws in active_workspaces(core, now_ms) {
            if let Some(Some(g)) = guard(|| s.git_context(Path::new(&ws))) {
                inputs.git.push((ws, g));
            }
        }
        inputs.offers = fresh_offers(core, guard(|| s.candidates()).unwrap_or_default(), now_ms);
    }
    inputs.memory = core.mind.as_ref().and_then(|m| m.stats().ok());
    let (news, profile) = memory_news(core);
    inputs.memory_news = news;
    let mut b = brief::compose(&inputs);
    if !b.is_quiet() {
        if let Some(text) = polish(core, &inputs, &b, &gpu) {
            b.spoken = brief::spoken(&text);
            b.text = text;
        }
    }
    (inputs, b, profile)
}

/// Result files behind the metric notices, newest first, each beside its previous sibling.
fn results_since(s: &marlowe_sentinel::Sentinel, notices: &[Notice]) -> Vec<brief::ResultComparison> {
    const MAX_RESULTS: usize = 3;
    let mut files: Vec<(&str, i64)> = Vec::new();
    for n in notices.iter().filter(|n| n.source == NoticeSource::Metric && n.kind == "result") {
        match files.iter_mut().find(|(f, _)| *f == n.subject) {
            Some(f) => f.1 = f.1.max(n.at_ms),
            None => files.push((&n.subject, n.at_ms)),
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.into_iter().take(MAX_RESULTS).filter_map(|(f, _)| guard(|| s.result_comparison(Path::new(f))).flatten()).collect()
}

/// Profile memories not in the last delivered brief's set, split by whether the user said them,
/// and the current set (stored once the brief is delivered). The first look is a baseline.
fn memory_news(core: &Core) -> (MemoryNews, Option<Vec<String>>) {
    let Some(mind) = &core.mind else { return (MemoryNews::default(), None) };
    let Some(Ok(profile)) = guard(|| mind.profile(marlowe_mind::ProfileQuery::default())) else {
        return (MemoryNews::default(), None);
    };
    let mut news = MemoryNews::default();
    if let Some(seen) = core.store.kv(PROFILE_SEEN).ok().flatten() {
        let seen: Vec<&str> = seen.lines().collect();
        for id in profile.ids.iter().filter(|id| !seen.contains(&id.as_str())) {
            match guard(|| mind.get(id)).and_then(Result::ok).flatten() {
                Some(h) if h.trust == marlowe_proto::MemoryTrust::User => news.from_user += 1,
                Some(_) => news.other += 1,
                None => {}
            }
        }
    }
    (news, Some(profile.ids))
}

/// One call on the Quick slot (else the Conductor) in the briefing block's register. Skipped on a
/// local model unless allowed and the GPU is free (see [`brief::polish_allowed`]).
fn polish(core: &Core, inputs: &BriefInputs, b: &Brief, gpu: &GpuReport) -> Option<String> {
    let parts = guard(|| core.brains.agent_parts(&Slot::Quick).or_else(|_| core.brains.agent_parts(&Slot::Conductor)))?;
    let (provider, info, _effort) = parts.ok()?;
    if !brief::polish_allowed(provider.kind(), core.cfg.local_polish, &inputs.runs, gpu) {
        return None;
    }
    let system = SystemPrompt::new().block(PersonaBlock::Briefing).user(core.user()).render();
    let req = brief::polish_request(&info.id, system, inputs, b);
    let attention: Vec<String> = b.items.iter().filter(|i| i.kind == BriefItemKind::Attention).map(|i| i.text.clone()).collect();
    brief::polish(provider, req, &b.text, &attention, POLISH_TIMEOUT)
}

/// Distinct workspaces of sessions updated in the last week, newest first.
fn active_workspaces(core: &Core, now_ms: i64) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in core.store.list(20).unwrap_or_default() {
        if s.updated_ms < now_ms - ACTIVE_WORKSPACE_MS {
            break;
        }
        if let Some(w) = s.workspace {
            if !out.contains(&w) {
                out.push(w);
            }
        }
        if out.len() == MAX_GIT_WORKSPACES {
            break;
        }
    }
    out
}

/// Candidates not offered in the last few days: live runs first, then most recent.
fn fresh_offers(core: &Core, mut candidates: Vec<WatchCandidate>, now_ms: i64) -> Vec<WatchCandidate> {
    candidates.retain(|c| {
        let at = core.store.kv(&format!("{OFFERED}{}", c.path)).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        at.is_none_or(|t| now_ms - t >= OFFER_AGAIN_MS)
    });
    candidates.sort_by(|a, b| b.live_runs.cmp(&a.live_runs).then(b.modified_ms.cmp(&a.modified_ms)));
    candidates.truncate(MAX_OFFERS);
    candidates
}

fn mark_offered(core: &Core, offers: &[WatchCandidate]) {
    let now = now_ms().to_string();
    for c in offers {
        let _ = core.store.set_kv(&format!("{OFFERED}{}", c.path), &now);
    }
}

/// Stores the brief as an assistant message in the day's session, creating it on the first
/// brief of the day. A day session that is live in a runner is left alone (its conversation is
/// in memory); a fresh one is started instead.
fn store_brief(core: &Core, text: &str) -> Result<SessionId, String> {
    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let existing = core.store.kv(DAY_SESSION)?.and_then(|v| {
        let (day, id) = v.split_once(' ')?;
        (day == today).then(|| SessionId(id.to_string()))
    });
    let reusable = match existing {
        Some(id) if !core.is_live(&id) && core.store.get(&id)?.is_some() => Some(id),
        _ => None,
    };
    let at = now_ms();
    let id = match reusable {
        Some(id) => id,
        None => {
            let id = SessionId::new_random();
            let title = format!("Brief · {}", now.format("%A %-d %B"));
            core.store.create(SessionSummary {
                id: id.clone(),
                title: title.clone(),
                workspace: None,
                created_ms: at,
                updated_ms: at,
                turns: 0,
            })?;
            core.store.set_kv(DAY_SESSION, &format!("{today} {}", id.as_str()))?;
            core.bus.publish(Event::SessionOpened { session: id.clone(), title, workspace: None });
            id
        }
    };
    core.store.append(&id, vec![Message::assistant(text)], 0, false, at)?;
    Ok(id)
}

fn quiet_hours(core: &Core) -> QuietHours {
    core.sentinel.as_ref().and_then(|s| guard(|| s.quiet_hours())).unwrap_or_default()
}

fn local_minute() -> u16 {
    let t = Local::now();
    (t.hour() * 60 + t.minute()) as u16
}

/// Speaks `text` when voice is present and it is outside quiet hours. A disabled or failing
/// voice is silent; the written brief already went out.
fn speak(core: &Core, text: &str) {
    let Some(v) = &core.voice else { return };
    if text.trim().is_empty() || quiet_hours(core).contains(local_minute()) {
        return;
    }
    let _ = guard(|| v.speak(text));
}

pub(crate) fn speak_notice(core: &Arc<Core>, n: &Notice) {
    if core.voice.is_none() || core.uptime() < NOTICE_SPEECH_AFTER_START {
        return;
    }
    if !marlowe_sentinel::notice::should_speak(n, quiet_hours(core), local_minute()) {
        return;
    }
    if !core.presence.take_turn(n, now_ms()) {
        return;
    }
    let title = n.title.trim().trim_end_matches('.');
    let line = match core.user().address.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{}, {title}.", capitalize(a)),
        None => format!("{title}."),
    };
    speak(core, &brief::spoken(&line));
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(id: &str, severity: Severity, detail: &str) -> Notice {
        Notice {
            id: id.into(),
            at_ms: 0,
            source: NoticeSource::Log,
            severity,
            kind: "nan".into(),
            title: "train.log logged NaN/inf".into(),
            detail: detail.into(),
            subject: "train.log".into(),
            seen: false,
        }
    }

    #[test]
    fn a_notice_is_said_once_and_again_only_when_it_escalates() {
        let p = Presence::default();
        let gap = NOTICE_SPEECH_GAP_MS;
        assert!(p.take_turn(&notice("n1", Severity::Attention, "step 10"), gap));
        // Re-raised 30 min later with a new detail: shown, not said.
        assert!(!p.take_turn(&notice("n1", Severity::Attention, "step 900"), 100 * gap));
        // Escalated: said.
        assert!(p.take_turn(&notice("n1", Severity::Alarm, "step 950"), 200 * gap));
        assert!(!p.take_turn(&notice("n1", Severity::Alarm, "step 990"), 300 * gap));
        // Another notice inside the gap waits; after it, it is said.
        assert!(!p.take_turn(&notice("n2", Severity::Alarm, ""), 200 * gap + 1));
        assert!(p.take_turn(&notice("n2", Severity::Alarm, ""), 400 * gap));
    }
}
