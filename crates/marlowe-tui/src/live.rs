//! The live strip's state: what the turn in flight is doing from Enter to the last token, and the
//! memory-write tail after it. Folded from the daemon's phase events and the stream itself; the
//! renderer ([`crate::render::strip`]) only reads it.
//!
//! Every number here is one the daemon or the stream reported, or is flagged as an estimate
//! (`*_est`) until the real one replaces it. Nothing waits on a timer: a line shows the frame after
//! its event arrives, and a stage that starts and ends inside one frame is still drawn once
//! ([`Flip`]).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Instant;

use marlowe_proto::{PhaseEvent, PhaseKind, PhaseState};

/// Rows the strip takes while it is up. A constant: the height never depends on what it shows.
pub const ROWS: u16 = 2;
/// How long a finished line (loaded, compacted, memory written) stays after the turn is over.
/// Display only: no work waits for it.
const LINGER_MS: i64 = 1_500;
/// A memory write that never reports its end stops showing after this.
const TAIL_STALE_MS: i64 = 6_000;
/// The window the token rate is taken over.
const RATE_WINDOW_MS: i64 = 1_000;

/// Things to show in order, each for at least one frame. Events can arrive faster than frames are
/// drawn (a recall that runs its nine stages in a millisecond); the queue keeps them and each frame
/// takes the next, so every stage is on screen once.
#[derive(Debug, Clone)]
pub struct Flip<T> {
    q: VecDeque<(T, Option<u64>)>,
}

impl<T> Default for Flip<T> {
    fn default() -> Self {
        Flip { q: VecDeque::new() }
    }
}

impl<T> Flip<T> {
    const CAP: usize = 48;

    pub fn push(&mut self, t: T) {
        if self.q.len() >= Self::CAP {
            self.q.pop_front();
        }
        self.q.push_back((t, None));
    }

    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.q.back_mut().map(|(t, _)| t)
    }

    pub fn find_mut(&mut self, f: impl Fn(&T) -> bool) -> Option<&mut T> {
        self.q.iter_mut().rev().map(|(t, _)| t).find(|t| f(t))
    }

    pub fn last(&self) -> Option<&T> {
        self.q.back().map(|(t, _)| t)
    }

    /// What frame `frame` shows. Asking again within the same frame gives the same answer.
    pub fn show(&mut self, frame: u64) -> Option<&T> {
        if self.q.len() > 1 && self.q.front().is_some_and(|(_, seen)| seen.is_some_and(|f| f < frame)) {
            self.q.pop_front();
        }
        let front = self.q.front_mut()?;
        front.1.get_or_insert(frame);
        Some(&front.0)
    }

    /// Something is still waiting for its frame, or the queue holds more than the one on screen.
    pub fn pending(&self) -> bool {
        self.q.len() > 1 || self.q.front().is_some_and(|(_, seen)| seen.is_none())
    }

    pub fn clear(&mut self) {
        self.q.clear();
    }
}

/// What one model call read: the tokens it took in fresh, the ones served from the cache, how long
/// it took. Flags say which of those are still estimates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Read {
    pub new: u64,
    pub cached: u64,
    pub ms: Option<u64>,
    pub new_est: bool,
    pub ms_est: bool,
    pub cached_est: bool,
    /// A later step of the turn: shown as `+1.2k`.
    pub later: bool,
}

/// One recall stage as the strip shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct StageRun {
    pub name: String,
    pub detail: Option<String>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub rate: Option<f32>,
    pub unit: Option<String>,
    pub started: i64,
    pub ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecallItem {
    Stage(StageRun),
    /// Recall is over: the row is free for what comes next.
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRun {
    pub call_id: String,
    pub summary: String,
    pub since: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wait {
    pub detail: String,
    pub since: i64,
}

/// A cold model load, from the engine's runner memory growing.
#[derive(Debug, Clone, PartialEq)]
pub struct Load {
    pub model: String,
    pub done_mib: u64,
    pub total_mib: Option<u64>,
    /// The total is the planner's prediction.
    pub est: bool,
    pub started: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub model: String,
    pub mib: Option<u64>,
    pub ms: u64,
    /// Shown until this; `None` while the turn is up.
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Compact {
    pub started: i64,
    /// The prompt's size when it was found too big.
    pub before: u64,
    pub written: u64,
    /// The summary's output cap: an upper bound, so the figure is an estimate.
    pub cap: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    pub before: u64,
    pub after: u64,
    pub ms: u64,
    pub failed: bool,
    pub until: Option<i64>,
}

/// The memory ingest and embed after a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Tail {
    pub turn: u32,
    pub ingested: u64,
    /// Embedding in progress: done and total.
    pub embedding: Option<(u64, u64)>,
    /// Embedded: how many vectors were installed and how long it took.
    pub embedded: Option<(u64, u64)>,
    pub indexed: bool,
    pub last: i64,
    pub done_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prefill {
    pub started: i64,
    /// The whole prompt, estimated, from the start event.
    pub est_total: u64,
    pub done: u64,
    /// The tokens it reads fresh, estimated, once the daemon has a stored rate.
    pub total: Option<u64>,
    pub rate: Option<f32>,
}

/// Rolling token rate.
#[derive(Debug, Clone, Default)]
pub struct TokWindow {
    samples: VecDeque<(i64, u64)>,
}

impl TokWindow {
    pub fn push(&mut self, at: i64, tokens: u64) {
        self.samples.push_back((at, tokens));
        while self.samples.front().is_some_and(|(t, _)| at - *t > RATE_WINDOW_MS) {
            self.samples.pop_front();
        }
    }

    /// Tokens a second over the last second, once two samples say how long it took.
    pub fn rate(&self, now: i64) -> Option<u64> {
        let live: Vec<&(i64, u64)> = self.samples.iter().filter(|(t, _)| now - *t <= RATE_WINDOW_MS).collect();
        let (first, last) = (live.first()?, live.last()?);
        let span = (last.0 - first.0).max(1) as f64 / 1000.0;
        if live.len() < 2 {
            return None;
        }
        let tokens: u64 = live.iter().skip(1).map(|(_, n)| *n).sum();
        Some((tokens as f64 / span).round() as u64)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Call {
    pub prefill: Option<Prefill>,
    pub read: Option<Read>,
    pub first_at: Option<i64>,
    pub think_since: Option<i64>,
    pub think_tokens: u64,
    pub write_since: Option<i64>,
    pub write_tokens: u64,
    pub window: TokWindow,
    /// The reasoning entry this call made, so its header can be corrected when the real read
    /// numbers arrive after it.
    pub entry: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Strip {
    /// Enter pressed, nothing back from the daemon yet.
    pub sending: Option<i64>,
    /// When the turn began for this window: Enter, or its first phase. Short phases show how long
    /// the turn has been going, so a wait with no stage of its own still ticks.
    pub began: Option<i64>,
    turn: Option<u32>,
    done_turn: Option<u32>,
    up: bool,
    short: RefCell<Flip<&'static str>>,
    recall: RefCell<Flip<RecallItem>>,
    pub call: Call,
    calls: u32,
    pub tools: Vec<ToolRun>,
    pub wait: Option<Wait>,
    pub load: Option<Load>,
    pub loaded: Option<Loaded>,
    pub compact: Option<Compact>,
    pub compacted: Option<Compacted>,
    pub tail: Option<Tail>,
    fit_tokens: u64,
    /// When the engine began reading: the send, or the end of the model load.
    pub(crate) read_from: Option<i64>,
    frame: Cell<u64>,
    first_token: Option<Instant>,
    screen_us: Cell<Option<u64>>,
}

impl Strip {
    // ---- the frame clock --------------------------------------------------------------------

    /// A frame starts: stages queued for display take their turn.
    pub fn begin_frame(&self) {
        let f = self.frame.get() + 1;
        self.frame.set(f);
        // The queues move on whether or not this frame draws the strip, so they never pile up
        // behind a view that hides it.
        self.recall.borrow_mut().show(f);
        self.short.borrow_mut().show(f);
    }

    pub fn frame(&self) -> u64 {
        self.frame.get()
    }

    /// The first token of the turn reached the screen: the TUI's share of token-to-screen.
    pub fn drawn(&self) {
        if let (Some(t), None) = (self.first_token, self.screen_us.get()) {
            self.screen_us.set(Some(t.elapsed().as_micros() as u64));
        }
    }

    /// Microseconds from the first token being applied to it being drawn.
    pub fn screen_us(&self) -> Option<u64> {
        self.screen_us.get()
    }

    // ---- whether the strip is up --------------------------------------------------------------

    /// A turn is in flight or about to be.
    pub fn turn_up(&self) -> bool {
        self.up || self.sending.is_some()
    }

    /// Anything the strip still has to show once the turn is over.
    pub fn trailing(&self, now: i64) -> bool {
        self.recall.borrow().pending()
            || self.loaded.as_ref().is_some_and(|l| l.until.is_some_and(|u| now < u))
            || self.compacted.as_ref().is_some_and(|c| c.until.is_some_and(|u| now < u))
            || self.tail_showing(now)
    }

    pub fn tail_showing(&self, now: i64) -> bool {
        match &self.tail {
            Some(t) => match t.done_at {
                Some(d) => self.up || now < d + LINGER_MS,
                None => now < t.last + TAIL_STALE_MS,
            },
            None => false,
        }
    }

    // ---- turn bookkeeping ---------------------------------------------------------------------

    /// Enter was pressed: the strip is up this frame, before the daemon has said anything.
    pub fn enter(&mut self, now: i64) {
        self.reset_turn();
        self.turn = None;
        self.done_turn = None;
        self.up = true;
        self.sending = Some(now);
        self.began = Some(now);
        self.short.borrow_mut().push("sending");
    }

    fn reset_turn(&mut self) {
        self.short.borrow_mut().clear();
        self.recall.borrow_mut().clear();
        self.call = Call::default();
        self.calls = 0;
        self.tools.clear();
        self.wait = None;
        self.load = None;
        self.loaded = None;
        self.compact = None;
        self.compacted = None;
        self.fit_tokens = 0;
        self.read_from = None;
        self.first_token = None;
        self.screen_us.set(None);
    }

    fn note_turn(&mut self, turn: u32) {
        if self.turn.is_some() && self.turn != Some(turn) {
            self.reset_turn();
        }
        self.turn = Some(turn);
        self.up = true;
        self.sending = None;
    }

    /// Events of a turn already finished (a replay after a reconnect) change nothing.
    fn fresh(&self, turn: u32) -> bool {
        self.done_turn.is_none_or(|d| turn > d)
    }

    pub fn turn_started(&mut self, turn: u32) {
        // A rewind numbers turns again from lower down: a turn that starts is always current.
        self.done_turn = self.done_turn.filter(|d| *d < turn);
        self.note_turn(turn);
        // Accepted: it is no longer being sent, whatever the daemon goes on to say.
        if self.short.borrow().last() == Some(&"sending") {
            self.flash("working");
        }
    }

    /// The turn is over: what it was doing stops; what trails it (the memory tail, a load or
    /// compaction result) lingers for a moment.
    pub fn finished(&mut self, turn: u32, at: i64) {
        self.done_turn = Some(turn);
        self.up = false;
        self.sending = None;
        self.began = None;
        self.call = Call::default();
        self.tools.clear();
        self.wait = None;
        self.load = None;
        self.compact = None;
        self.short.borrow_mut().clear();
        if let Some(l) = &mut self.loaded {
            l.until = Some(at + LINGER_MS);
        }
        if let Some(c) = &mut self.compacted {
            c.until = Some(at + LINGER_MS);
        }
    }

    /// The turn failed before it began, or the session changed under it.
    pub fn reset(&mut self) {
        self.reset_turn();
        self.up = false;
        self.sending = None;
        self.began = None;
        self.turn = None;
        self.tail = None;
        self.done_turn = None;
    }

    // ---- the model's output -------------------------------------------------------------------

    fn first_output(&mut self, at: i64) {
        if self.first_token.is_none() {
            self.first_token = Some(Instant::now());
        }
        if self.call.first_at.is_some() {
            return;
        }
        self.call.first_at = Some(at);
        if self.call.read.is_some() {
            return;
        }
        let Some(p) = self.call.prefill else { return };
        // The first token ends the read. Until the engine's own numbers come in with the call's
        // usage, the read is what we estimated, over the time it actually took.
        let new = p.total.unwrap_or(p.est_total);
        let cached = p.total.map_or(0, |t| p.est_total.saturating_sub(t));
        let from = self.read_from.unwrap_or(p.started);
        self.call.read = Some(Read {
            new,
            cached,
            ms: Some((at - from).max(0) as u64),
            new_est: true,
            ms_est: true,
            cached_est: true,
            later: self.calls > 1,
        });
    }

    pub fn reasoning(&mut self, at: i64, tokens: u64) {
        self.first_output(at);
        self.call.think_since.get_or_insert(at);
        self.call.think_tokens += tokens;
        self.call.window.push(at, tokens);
    }

    pub fn text(&mut self, at: i64, chars: usize) {
        self.first_output(at);
        self.call.write_since.get_or_insert(at);
        let tokens = (chars as u64).div_ceil(4);
        self.call.write_tokens += tokens;
        self.call.window.push(at, tokens);
    }

    /// What the reasoning entry made now should carry.
    pub fn call_read(&self) -> Option<Read> {
        self.call.read
    }

    pub fn tool_started(&mut self, call_id: &str, summary: &str, at: i64) {
        self.calls = self.calls.max(1);
        self.call = Call::default();
        self.tools.retain(|t| t.call_id != call_id);
        self.tools.push(ToolRun { call_id: call_id.to_string(), summary: summary.to_string(), since: at });
    }

    pub fn tool_finished(&mut self, call_id: &str) {
        self.tools.retain(|t| t.call_id != call_id);
    }

    // ---- recall -------------------------------------------------------------------------------

    /// The Recalled event: the stages are over, whatever the frames have not shown yet still plays.
    pub fn recalled(&mut self) {
        self.recall_done();
    }

    fn recall_done(&self) {
        let mut q = self.recall.borrow_mut();
        if q.last() != Some(&RecallItem::Done) {
            q.push(RecallItem::Done);
        }
    }

    /// The recall item this frame shows.
    pub fn recall_item(&self) -> Option<RecallItem> {
        self.recall.borrow_mut().show(self.frame.get()).cloned()
    }

    /// The short phase this frame shows.
    pub fn short_label(&self) -> Option<&'static str> {
        self.short.borrow_mut().show(self.frame.get()).copied()
    }

    fn flash(&self, label: &'static str) {
        let mut q = self.short.borrow_mut();
        if q.last() != Some(&label) {
            q.push(label);
        }
    }

    // ---- phases -------------------------------------------------------------------------------

    /// Folds one phase event in. Returns the read numbers when a model call's real ones arrive.
    pub fn phase(&mut self, p: &PhaseEvent, at: i64) -> Option<Read> {
        if p.run.is_some() {
            return None;
        }
        if p.kind == PhaseKind::MemoryWrite {
            self.memory_write(p, at);
            return None;
        }
        if !self.fresh(p.turn) {
            return None;
        }
        self.began.get_or_insert(at);
        self.note_turn(p.turn);
        let stage = p.stage.as_deref();
        match (p.kind, p.state) {
            (PhaseKind::Sending, PhaseState::Start) => self.flash("sending"),
            (PhaseKind::Setup, PhaseState::Start) => self.flash("setting up"),
            (PhaseKind::Prompt, PhaseState::Start) => self.flash("building the prompt"),
            (PhaseKind::Fit, PhaseState::Start) => self.flash("fitting the context"),
            (PhaseKind::Fit, PhaseState::End) => self.fit_tokens = p.done.unwrap_or(0),
            (PhaseKind::Admit, _) if stage == Some("gpu") => match p.state {
                PhaseState::Start => {
                    self.wait = Some(Wait { detail: p.detail.clone().unwrap_or_else(|| "waiting for the GPU".into()), since: at })
                }
                PhaseState::End => self.wait = None,
                PhaseState::Progress => {}
            },
            (PhaseKind::Admit, PhaseState::Start) => self.flash("checking the GPU"),
            (PhaseKind::Wire, PhaseState::Start) => self.flash("calling the model"),
            (PhaseKind::Recall, _) if stage.is_none() => match p.state {
                PhaseState::Start => self.flash("recalling"),
                PhaseState::End => self.recall_done(),
                PhaseState::Progress => {}
            },
            (PhaseKind::Recall, _) => self.recall_stage(p, at),
            (PhaseKind::Compact, _) => self.compaction(p, at),
            (PhaseKind::Load, _) => self.loading(p, at),
            (PhaseKind::Prefill, _) => return self.prefill(p, at),
            _ => {}
        }
        None
    }

    fn recall_stage(&mut self, p: &PhaseEvent, at: i64) {
        let Some(name) = p.stage.as_deref() else { return };
        let mut q = self.recall.borrow_mut();
        let open = |i: &RecallItem| matches!(i, RecallItem::Stage(s) if s.name == name);
        match p.state {
            PhaseState::Start => {
                // A label for the stage just opened (the reranker saying cpu or gpu) fills it in.
                if let Some(RecallItem::Stage(s)) = q.last_mut() {
                    if s.name == name && s.ms.is_none() {
                        s.detail = p.detail.clone().or(s.detail.take());
                        return;
                    }
                }
                q.push(RecallItem::Stage(StageRun {
                    name: name.to_string(),
                    detail: p.detail.clone(),
                    done: None,
                    total: None,
                    rate: None,
                    unit: p.unit.clone(),
                    started: at,
                    ms: None,
                }));
            }
            PhaseState::Progress | PhaseState::End => {
                if let Some(RecallItem::Stage(s)) = q.find_mut(open) {
                    if p.state == PhaseState::Progress {
                        s.done = p.done;
                        s.total = p.total;
                        s.rate = p.rate;
                        s.unit = p.unit.clone().or(s.unit.take());
                    } else {
                        s.ms = Some(p.ms.unwrap_or(0));
                        s.total = p.total.or(s.total);
                        s.detail = p.detail.clone().or(s.detail.take());
                    }
                } else if p.state == PhaseState::End {
                    // A stage too quick to have been seen starting.
                    q.push(RecallItem::Stage(StageRun {
                        name: name.to_string(),
                        detail: p.detail.clone(),
                        done: None,
                        total: p.total,
                        rate: None,
                        unit: p.unit.clone(),
                        started: at,
                        ms: Some(p.ms.unwrap_or(0)),
                    }));
                }
            }
        }
    }

    fn compaction(&mut self, p: &PhaseEvent, at: i64) {
        match p.state {
            PhaseState::Start => {
                self.flash("compacting");
                self.compact = Some(Compact { started: at, before: self.fit_tokens, written: 0, cap: 0 });
            }
            PhaseState::Progress => {
                if let Some(c) = &mut self.compact {
                    c.written = p.done.unwrap_or(c.written);
                    c.cap = p.total.unwrap_or(c.cap);
                }
            }
            PhaseState::End => {
                let c = self.compact.take();
                let cancelled = p.detail.as_deref() == Some("cancelled");
                if cancelled {
                    return;
                }
                self.compacted = Some(Compacted {
                    before: p.done.or(c.as_ref().map(|c| c.before)).unwrap_or(0),
                    after: p.total.unwrap_or(0),
                    ms: p.ms.unwrap_or(0),
                    failed: p.detail.as_deref() == Some("failed"),
                    until: None,
                });
            }
        }
    }

    fn loading(&mut self, p: &PhaseEvent, at: i64) {
        match p.state {
            PhaseState::Start => {
                self.load = Some(Load {
                    model: p.detail.clone().unwrap_or_else(|| "the model".into()),
                    done_mib: 0,
                    total_mib: p.total,
                    est: p.est,
                    started: at,
                });
            }
            PhaseState::Progress => {
                if let Some(l) = &mut self.load {
                    l.done_mib = p.done.unwrap_or(l.done_mib).max(l.done_mib);
                    l.total_mib = p.total.or(l.total_mib);
                    l.est = p.est;
                }
            }
            PhaseState::End => {
                let l = self.load.take();
                self.read_from = Some(at);
                if let Some(l) = l {
                    self.loaded = Some(Loaded {
                        model: p.detail.clone().unwrap_or(l.model),
                        mib: p.done.filter(|d| *d > 0).or(Some(l.done_mib).filter(|d| *d > 0)),
                        ms: p.ms.unwrap_or(0),
                        until: None,
                    });
                }
            }
        }
    }

    fn prefill(&mut self, p: &PhaseEvent, at: i64) -> Option<Read> {
        match p.state {
            PhaseState::Start => {
                self.calls += 1;
                self.call = Call {
                    prefill: Some(Prefill { started: at, est_total: p.total.unwrap_or(0), done: 0, total: None, rate: None }),
                    ..Call::default()
                };
                self.read_from = self.load.is_none().then_some(at);
                None
            }
            PhaseState::Progress => {
                if let Some(f) = &mut self.call.prefill {
                    f.done = p.done.unwrap_or(f.done);
                    f.total = p.total.or(f.total);
                    f.rate = p.rate.or(f.rate);
                }
                None
            }
            PhaseState::End => {
                let new = p.done?;
                let read = Read {
                    new,
                    cached: p.total.unwrap_or(new).saturating_sub(new),
                    ms: p.ms.or(self.call.read.and_then(|r| r.ms)),
                    new_est: false,
                    ms_est: p.ms.is_none(),
                    cached_est: p.est,
                    later: self.calls > 1,
                };
                self.call.read = Some(read);
                Some(read)
            }
        }
    }

    fn memory_write(&mut self, p: &PhaseEvent, at: i64) {
        let fresh = Tail { turn: p.turn, ingested: 0, embedding: None, embedded: None, indexed: false, last: at, done_at: None };
        match &self.tail {
            Some(t) if t.turn == p.turn && t.done_at.is_none() => {}
            _ => self.tail = Some(fresh),
        }
        let Some(t) = &mut self.tail else { return };
        t.last = at;
        match (p.stage.as_deref(), p.state) {
            (Some("ingest"), PhaseState::End) => t.ingested += p.done.unwrap_or(1),
            (Some("embed"), PhaseState::Start) => t.embedding = Some((0, p.total.unwrap_or(0))),
            (Some("embed"), PhaseState::Progress) => t.embedding = Some((p.done.unwrap_or(0), p.total.unwrap_or(0))),
            (Some("embed"), PhaseState::End) => {
                t.embedding = None;
                t.embedded = Some((p.done.unwrap_or(0), p.ms.unwrap_or(0)));
            }
            (Some("index"), PhaseState::End) => {
                t.indexed = true;
                t.done_at = Some(at);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_queued_item_gets_a_frame() {
        let mut f: Flip<u32> = Flip::default();
        for i in 0..5 {
            f.push(i);
        }
        let mut seen = Vec::new();
        for frame in 1..=8 {
            seen.push(*f.show(frame).unwrap());
            // Asked twice in one frame: the same answer.
            assert_eq!(f.show(frame), seen.last());
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 4, 4, 4], "one frame each, then the last stays");
        assert!(!f.pending());
    }

    #[test]
    fn a_lone_item_stays_and_an_unseen_one_is_pending() {
        let mut f: Flip<u32> = Flip::default();
        f.push(7);
        assert!(f.pending());
        assert_eq!(f.show(1), Some(&7));
        assert!(!f.pending());
        assert_eq!(f.show(9), Some(&7));
    }

    fn ev(kind: PhaseKind, state: PhaseState) -> PhaseEvent {
        PhaseEvent { kind, state, turn: 1, ..Default::default() }
    }

    #[test]
    fn the_first_token_turns_the_estimate_into_an_estimated_read_and_the_engine_replaces_it() {
        let mut s = Strip::default();
        let mut start = ev(PhaseKind::Prefill, PhaseState::Start);
        start.total = Some(8_200);
        start.est = true;
        s.phase(&start, 1_000);
        let mut p = ev(PhaseKind::Prefill, PhaseState::Progress);
        p.done = Some(3_000);
        p.total = Some(7_000);
        p.rate = Some(1_900.0);
        s.phase(&p, 1_500);
        assert_eq!(s.call.prefill.map(|f| (f.done, f.total)), Some((3_000, Some(7_000))));
        assert!(s.call_read().is_none(), "nothing is read yet");
        s.reasoning(2_900, 4);
        let est = s.call_read().expect("the first token ends the read");
        assert_eq!((est.new, est.cached, est.ms), (7_000, 1_200, Some(1_900)));
        assert!(est.new_est && est.ms_est && est.cached_est, "all of it is still an estimate");
        let mut end = ev(PhaseKind::Prefill, PhaseState::End);
        end.done = Some(6_950);
        end.total = Some(8_150);
        end.ms = Some(1_873);
        let real = s.phase(&end, 9_000).expect("the engine's numbers come back");
        assert_eq!((real.new, real.cached, real.ms), (6_950, 1_200, Some(1_873)));
        assert!(!real.new_est && !real.ms_est, "measured now");
    }

    #[test]
    fn recall_stages_queue_progress_lands_on_the_open_stage_and_done_closes_the_row() {
        let mut s = Strip::default();
        let stage = |name: &str, state| PhaseEvent { stage: Some(name.into()), ..ev(PhaseKind::Recall, state) };
        s.phase(&stage("dense", PhaseState::Start), 10);
        let mut p = stage("dense", PhaseState::Progress);
        p.done = Some(2_940);
        p.total = Some(4_198);
        p.rate = Some(1.9e6);
        s.phase(&p, 12);
        let mut end = stage("dense", PhaseState::End);
        end.ms = Some(4);
        s.phase(&end, 14);
        s.phase(&stage("gate", PhaseState::Start), 14);
        s.recalled();
        s.begin_frame();
        let Some(RecallItem::Stage(first)) = s.recall_item() else { panic!("dense first") };
        assert_eq!((first.name.as_str(), first.done, first.ms), ("dense", Some(2_940), Some(4)));
        s.begin_frame();
        assert!(matches!(s.recall_item(), Some(RecallItem::Stage(g)) if g.name == "gate"));
        s.begin_frame();
        assert_eq!(s.recall_item(), Some(RecallItem::Done));
    }

    #[test]
    fn a_finished_turns_late_events_change_nothing_and_a_rewound_turn_is_current_again() {
        let mut s = Strip::default();
        s.turn_started(5);
        s.finished(5, 100);
        s.phase(&PhaseEvent { turn: 5, ..ev(PhaseKind::Prompt, PhaseState::Start) }, 110);
        assert!(!s.turn_up(), "a replayed event of a finished turn does not bring the strip back");
        s.turn_started(2);
        assert!(s.turn_up(), "after a rewind the turn numbers start lower down");
    }

    #[test]
    fn the_memory_tail_counts_ingests_and_completes_on_the_index() {
        let mut s = Strip::default();
        let mw = |stage: &str, state| PhaseEvent { stage: Some(stage.into()), ..ev(PhaseKind::MemoryWrite, state) };
        let mut ing = mw("ingest", PhaseState::End);
        ing.done = Some(1);
        s.phase(&ing, 10);
        s.phase(&ing, 20);
        let mut emb = mw("embed", PhaseState::Start);
        emb.total = Some(2);
        s.phase(&emb, 30);
        assert!(s.tail_showing(40));
        let mut done = mw("embed", PhaseState::End);
        done.done = Some(2);
        done.ms = Some(31);
        s.phase(&done, 60);
        s.phase(&mw("index", PhaseState::End), 70);
        let t = s.tail.as_ref().unwrap();
        assert_eq!((t.ingested, t.embedded, t.indexed), (2, Some((2, 31)), true));
        assert!(s.tail_showing(70 + LINGER_MS - 1));
        assert!(!s.tail_showing(70 + LINGER_MS + 1), "it lingers briefly and goes");
        // One that never reports its end is not shown for ever.
        let mut stuck = Strip::default();
        stuck.phase(&ing, 0);
        assert!(!stuck.tail_showing(TAIL_STALE_MS + 1));
    }

    #[test]
    fn the_token_rate_needs_two_samples_and_forgets_the_old() {
        let mut w = TokWindow::default();
        assert_eq!(w.rate(0), None);
        w.push(0, 4);
        assert_eq!(w.rate(0), None);
        w.push(500, 20);
        assert_eq!(w.rate(500), Some(40));
        assert_eq!(w.rate(5_000), None, "samples older than the window do not count");
    }
}
