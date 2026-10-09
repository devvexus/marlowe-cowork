//! The scripted session. No model, no memory, no network.
//!
//! Fixed responses, fixed tool-call sequences, deterministic timing, and a way to drive every
//! status state on demand. Craft is proven *before* there is an agent behind it, because craft
//! retrofitted onto a working agent never happens.
//!
//! # The prose here carries the persona
//!
//! Addendum C applies to *anything producing user-visible prose*, and a stub's scripted output is
//! user-visible prose. Every line below is checked against §C1 and §C4: the first sentence carries
//! the answer including when the answer is no, bad news is stated plainly and first, there is no
//! opening compliment, no eagerness, no restating the question, and no emoji.
//!
//! `"Your cost base moved."` is the shape. `"Great question! Let me look into that for you."` is
//! the shape this file exists to never contain.
//!
//! # Determinism
//!
//! Every beat is scheduled at an offset from the moment it was queued, so a session driven by a
//! virtual clock replays identically. That is what lets the §B13 suite render frame N and frame
//! N+1 and diff them.

use crate::amplitude;
use marlowe_view::meter::{Frame, MeterSource, BASELINE};
use marlowe_view::approval::{BlastRadius, Ceiling, Effect, FirstTime, Medium, Novelty, Offered, RiskTier};
use marlowe_view::model::*;
use marlowe_view::notice::{Echo, Speech};
use marlowe_view::turn::{DegradedPath, Metric, ResultSummary, ToolLineState};
use marlowe_view::SessionView;

/// One scheduled change to the session.
#[derive(Debug, Clone, PartialEq)]
struct Beat {
    at_ms: u64,
    action: Action,
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Status(StatusState, &'static str, &'static [&'static str]),
    Say(&'static str),
    StartTools,
    Tool(ToolCall),
    /// Resolve the running call with this id.
    Finish(u64, ToolLineState),
    Approval(BlastRadius),
    Compact(u32),
    Degrade(DegradedPath),
}

/// The whole stub. One value, no background threads, no I/O.
///
/// **C2d: this is a producer, and the thing it produces is [`SessionView`].** It owns one and
/// mutates it — which is a producer's privilege and not a surface's. `view()` hands out a
/// read-only borrow; the surface takes a snapshot of it and cannot write back.
///
/// What is *not* here any more: `tab`. Which inspector tab you are looking at is a property of
/// looking, not of the session, so it moved to `App`.
#[derive(Debug)]
pub struct Session {
    view: SessionView,
    /// The meter's last reported frame.
    ///
    /// **Held, not recomputed, when the source stops.** This field *is* ADR-021's freeze: `tick`
    /// only writes it when `amplitude::sample` returns `Some`, and publishes `MeterSource::None`
    /// otherwise, so `waiting` leaves whatever was last true standing on screen.
    last_meter: Frame,
    pending: Vec<Beat>,
    next_call_id: u64,
    /// Set when `running` so the meter can report elapsed against expected (§B5).
    run_started_ms: u64,
    run_expected_ms: u64,
    last_tick_ms: u64,
}

impl Session {
    /// The opening state, matching the mockup: a session mid-conversation with real history behind
    /// it, because an empty transcript proves nothing about how a full one renders.
    pub fn new() -> Self {
        let mut s = Self {
            view: SessionView {
                // Empty rather than absent: the pane exists and has nothing in it yet, which
                // is what a stubbed session honestly is.
                status_pane: Vec::new(),
                control: ControlStrip {
                    model: Picker::new(&["opus-5", "sonnet-5", "haiku-4.5", "local/qwen-32b"], 0),
                    profile: Picker::new(&["work", "personal"], 0),
                    session: Picker::new(
                        &["thursday", "ingest-debug", "q3-research", "marlowe-m1"],
                        0,
                    ),
                    workspace: Picker::new(
                        &["~/projects/ingest", "~/projects/marlowe", "~/notes"],
                        0,
                    ),
                    autonomy: Picker::new(&["observe", "suggest", "draft", "confirm", "act"], 2),
                    // **The daemon's list, read from the crate both can reach.** This was a
                    // second literal -- `["ollama", "openrouter"]` -- and it was already a
                    // provider out of date, because `marlowe-stub` structurally cannot depend on
                    // `marlowe-daemon` (the C2d acceptance) and so could never have been kept in
                    // step from either end. The list moved to `marlowe-view`, which both crates
                    // already depend on.
                    provider: Picker::new(marlowe_view::provider::PROVIDERS, 0),
                },
                status: StatusBand {
                    state: StatusState::Listening,
                    detail: "voice · barge-in on · say \"marlowe\" to interrupt".into(),
                    figures: vec!["740 ms voice-to-voice".into(), "opus-5 · work".into()],
                    degraded: None,
                    // The stub is a SECOND definition of the surface's shape, so a new field
                    // has to be added here too or every stub-driven surface test stops
                    // compiling. None: the stub scripts a conversation, not a live turn,
                    // and inventing a cadence here would put a number on a screen that no
                    // model produced.
                    cadence: None,
                },
                transcript: opening_transcript(),
                pager: Pager {
                    turn: 12,
                    compacted: 47,
                    lineage: 3,
                },
                ambient: Ambient {
                    fill_pct: 12,
                    spend_cents: 18,
                    elapsed_min: 22,
                },
                approval: None,
        pending_approval: None,
                meter: MeterSource::None,
                runs: runs_pane(),
                schedule: schedule_pane(),
            },
            last_meter: BASELINE,
            pending: Vec::new(),
            next_call_id: 100,
            run_started_ms: 0,
            run_expected_ms: 1_200_000,
            last_tick_ms: 0,
        };
        s.report_meter(amplitude::sample(s.view.status.state, 0, 0, 0));
        s
    }

    /// The published view. **Read-only, and the only thing a surface is given.**
    pub fn view(&self) -> &SessionView {
        &self.view
    }

    /// Record what the amplitude source reported, holding the last frame when it reports nothing.
    ///
    /// The `None` branch publishes `MeterSource::None` rather than the held frame, so a surface
    /// can tell "still, and live" from "frozen, because nothing is feeding it" — §B5 shows the
    /// user different things for the two and the type keeps them different all the way across.
    fn report_meter(&mut self, sampled: Option<Frame>) {
        match sampled {
            Some(f) => {
                self.last_meter = f;
                self.view.meter = MeterSource::Reported(f);
            }
            None => self.view.meter = MeterSource::None,
        }
    }

    /// The last frame a source actually reported. Exposed for the freeze test, which has to assert
    /// that the held frame survives the source stopping.
    pub fn last_meter(&self) -> Frame {
        self.last_meter
    }

    /// Advance to `now_ms`. Idempotent for a given time; safe to call every frame.
    pub fn tick(&mut self, now_ms: u64) {
        self.last_tick_ms = now_ms;

        let due: Vec<Beat> = {
            let (due, rest): (Vec<_>, Vec<_>) =
                self.pending.drain(..).partition(|b| b.at_ms <= now_ms);
            self.pending = rest;
            due
        };
        for beat in due {
            self.apply(beat.action, now_ms);
        }

        // Live tool lines animate in place with elapsed time (§B6).
        for entry in &mut self.view.transcript {
            if let Entry::Tools(calls) = entry {
                for call in calls {
                    if let ToolLineState::Running { elapsed_ms } = &mut call.state {
                        *elapsed_ms = now_ms.saturating_sub(self.run_started_ms);
                    }
                }
            }
        }

        // ADR-021: publish what the source reported, including that it reported nothing. `None`
        // holds the last frame, and that is the whole of `waiting`'s freeze.
        let sampled = amplitude::sample(
            self.view.status.state,
            now_ms,
            now_ms.saturating_sub(self.run_started_ms),
            self.run_expected_ms,
        );
        self.report_meter(sampled);
    }

    fn apply(&mut self, action: Action, now_ms: u64) {
        match action {
            Action::Status(state, detail, figures) => {
                self.set_state(state, detail, figures, now_ms);
            }
            Action::Say(text) => self.view.transcript.push(Entry::Said(Speech::Model(text.into()))),
            Action::StartTools => self.view.transcript.push(Entry::Tools(Vec::new())),
            Action::Tool(call) => self.push_tool(call),
            Action::Finish(id, state) => {
                for entry in &mut self.view.transcript {
                    if let Entry::Tools(calls) = entry {
                        for call in calls.iter_mut().filter(|c| c.id == id) {
                            // §B6: failures auto-expand. Set here rather than at render time so
                            // the user can collapse one back down and have it stay collapsed.
                            call.expanded = matches!(state, ToolLineState::Failed(_));
                            call.state = state.clone();
                        }
                    }
                }
            }
            Action::Approval(radius) => {
                self.view.approval = Some(radius);
                self.set_state(
                    StatusState::Waiting,
                    "approval needed · send email as you",
                    &["irreversible · ceiling tier", "↵ approve · esc deny"],
                    now_ms,
                );
            }
            Action::Compact(turns) => {
                self.view.transcript.push(Entry::Compacted { turns });
                self.view.pager.compacted += turns;
                self.view.pager.lineage += 1;
                self.view.ambient.fill_pct = 12;
            }
            Action::Degrade(path) => self.view.status.degraded = Some(path),
        }
    }

    fn push_tool(&mut self, call: ToolCall) {
        // §B6: consecutive same-verb calls collapse — six reads become `⋯ read  6 files`.
        if let Some(Entry::Tools(calls)) = self.view.transcript.last_mut() {
            if let Some(prev) = calls.last_mut() {
                let both_settled = !matches!(prev.state, ToolLineState::Running { .. })
                    && !matches!(call.state, ToolLineState::Running { .. });
                // A failure never collapses into a group. It has to stay individually visible and
                // individually expandable, which is the whole point of auto-expanding it.
                let collapsible = prev.verb == call.verb
                    && both_settled
                    && !prev.is_failure()
                    && !call.is_failure();
                if collapsible {
                    prev.collapsed.push(call.target.clone());
                    return;
                }
            }
            calls.push(call);
        } else {
            self.view.transcript.push(Entry::Tools(vec![call]));
        }
    }

    fn set_state(
        &mut self,
        state: StatusState,
        detail: &str,
        figures: &[&str],
        now_ms: u64,
    ) {
        if state == StatusState::Running && self.view.status.state != StatusState::Running {
            self.run_started_ms = now_ms;
        }
        self.view.status.state = state;
        self.view.status.detail = detail.to_string();
        self.view.status.figures = figures.iter().map(|s| (*s).to_string()).collect();
    }

    /// **Drive any status state on demand** — the `^v` cycle, and the classic CLI's `/status
    /// <state>`. Required by M1's scope: every state must be reachable without waiting for a
    /// script to arrive at it.
    pub fn force_state(&mut self, state: StatusState, now_ms: u64) {
        let (detail, figures): (&str, &[&str]) = match state {
            StatusState::Listening => (
                "voice · barge-in on · say \"marlowe\" to interrupt",
                &["740 ms voice-to-voice", "opus-5 · work"],
            ),
            StatusState::Thinking => (
                "reading 34 sources · 12 pending",
                &["deep-research · 4m 12s", "$1.20 of $3.00"],
            ),
            StatusState::Speaking => (
                "answering · interrupt any time",
                &["740 ms voice-to-voice", "opus-5 · work"],
            ),
            StatusState::Writing => (
                "report.md · 1,840 words so far",
                &["streaming", "$0.31 this turn"],
            ),
            StatusState::Running => (
                "bash · pytest tests/memory",
                &["0m 41s elapsed", "3 of 12 passed"],
            ),
            StatusState::Waiting => (
                "approval needed · send email as you",
                &["irreversible · ceiling tier", "↵ approve · esc deny"],
            ),
            StatusState::Idle => (
                "nothing running · 2 background runs elsewhere",
                &["daemon 6d 4h", "$6.31 today"],
            ),
        };
        self.set_state(state, detail, figures, now_ms);
        // `waiting` and the overlay are one situation, not two. §B5's seventh state *is* a pending
        // approval, so driving the state drives the overlay with it — otherwise the band would
        // claim the user owes an answer to a question nobody asked.
        self.view.approval = match state {
            StatusState::Waiting => Some(send_as_you()),
            _ => None,
        };
        self.run_expected_ms = 120_000;
        if state == StatusState::Running {
            self.run_started_ms = now_ms.saturating_sub(41_000);
        }
    }

    /// The next state in §B5's order. Bound to `^v`.
    pub fn cycle_state(&mut self, now_ms: u64) {
        const ORDER: [StatusState; 7] = [
            StatusState::Listening,
            StatusState::Thinking,
            StatusState::Speaking,
            StatusState::Writing,
            StatusState::Running,
            StatusState::Waiting,
            StatusState::Idle,
        ];
        let i = ORDER
            .iter()
            .position(|s| *s == self.view.status.state)
            .unwrap_or(0);
        self.force_state(ORDER[(i + 1) % ORDER.len()], now_ms);
    }

    /// A user message. Queues the scripted reply; returns immediately.
    ///
    /// **Input is never blocked** (§B10) — the caller may submit again while beats are pending,
    /// and queued messages are appended in order.
    pub fn submit(&mut self, text: &str, now_ms: u64) {
        self.view.transcript.push(Entry::User(text.to_string()));
        self.view.pager.turn += 1;
        self.view.ambient.fill_pct = (self.view.ambient.fill_pct + 4).min(99);
        self.view.ambient.spend_cents += 3;

        let id = self.next_call_id;
        self.next_call_id += 3;
        for beat in reply_for(text, id) {
            self.pending.push(Beat {
                at_ms: now_ms + beat.at_ms,
                action: beat.action,
            });
        }
        self.pending.sort_by_key(|b| b.at_ms);
    }

    /// True while scripted work is outstanding. The classic CLI drains on this.
    pub fn is_busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Milliseconds until the next beat, for the event-loop poll timeout. `None` means nothing is
    /// scheduled, so the loop may block until the user does something — which is how a terminal
    /// that is doing nothing costs nothing.
    pub fn next_beat_in(&self, now_ms: u64) -> Option<u64> {
        self.pending
            .iter()
            .map(|b| b.at_ms.saturating_sub(now_ms))
            .min()
    }

    /// §B10: `Esc` interrupts and **partial output is kept**.
    pub fn interrupt(&mut self, now_ms: u64) {
        self.pending.clear();
        for entry in &mut self.view.transcript {
            if let Entry::Tools(calls) = entry {
                for call in calls {
                    if matches!(call.state, ToolLineState::Running { .. }) {
                        // v1.0 §9: idempotent reads complete, mutations cancel. The stub's running
                        // calls are reads, so they complete rather than vanishing.
                        call.state = ToolLineState::Ok(ResultSummary::new(vec![Metric::State(
                            "interrupted",
                        )]));
                    }
                }
            }
        }
        self.force_state(StatusState::Idle, now_ms);
    }

    /// Resolve §B9's overlay. `approved` is recorded for the rubber-stamping measurement that
    /// v1.0 §14.8 requires; M1 has nowhere to record it yet, which is noted rather than faked.
    pub fn resolve_approval(&mut self, approved: bool, now_ms: u64) {
        self.view.approval = None;
        let line = if approved {
            "Sent. The thread is in your drafts folder if you want the copy."
        } else {
            "Not sent. It stays in drafts."
        };
        self.view.transcript.push(Entry::Said(Speech::Model(line.into())));
        self.force_state(StatusState::Idle, now_ms);
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// The scripted producer. **Every intent the surface can form, applied here and nowhere else.**
///
/// C2d moved these bodies out of `marlowe-surface`: `/undo`'s truncation, `/compact`'s push, the
/// picker assignment and `force_state` all used to run inside the command dispatcher, on a
/// producer the surface held mutably. They are the same code; what changed is which side of the
/// boundary it runs on.
impl marlowe_view::Produce for Session {
    fn view(&self) -> &SessionView {
        &self.view
    }

    fn apply(&mut self, intent: marlowe_view::Intent) -> Result<(), marlowe_view::IntentError> {
        use marlowe_view::{ControlId, Intent};
        let now_ms = self.last_tick_ms;
        match intent {
            Intent::Send(text) => self.submit(&text, now_ms),
            Intent::Interrupt => self.interrupt(now_ms),
            Intent::Approve { granted, .. } => self.resolve_approval(granted, now_ms),
            Intent::ForceState(state) => self.force_state(state, now_ms),
            // **Refused by name, not dropped.** The stub has no daemon, and a run is the
            // daemon's. A scripted `/steer` that appeared to work would be a demo of a control
            // that does nothing — the exact shape `IntentError` exists to prevent.
            // **The stub IS its own daemon**, so there is nothing to ask and nothing to refuse:
            // its `runs` are already the whole truth it has. Refusing here would print a refusal
            // on every `/runs` in the demo, which is a broken-looking surface reporting a working
            // one.
            Intent::Runs => {}
            Intent::Steer { .. } | Intent::Watch { .. } => {
                return Err(marlowe_view::IntentError::NotADemo(
                    "a run belongs to a daemon, and the stub has none",
                ))
            }
            Intent::Compact => {
                let turns = self.view.pager.turn.max(1);
                self.view.transcript.push(Entry::Compacted { turns });
                self.view.pager.compacted += turns;
                self.view.pager.lineage += 1;
                self.view.ambient.fill_pct = 12;
            }
            Intent::Undo(n) => {
                // §B10: soft-delete the last N turns, identically across TUI, CLI and messaging —
                // which is why it lives on the producer and not in either surface.
                let mut removed = 0;
                while removed < n {
                    let Some(start) = self
                        .view
                        .transcript
                        .iter()
                        .rposition(|e| matches!(e, Entry::User(_)))
                    else {
                        break;
                    };
                    self.view.transcript.truncate(start);
                    removed += 1;
                }
                self.view.pager.turn = self.view.pager.turn.saturating_sub(removed as u32);
            }
            Intent::Select { control, option } => {
                let p = match control {
                    ControlId::Model => &mut self.view.control.model,
                    ControlId::Profile => &mut self.view.control.profile,
                    ControlId::Session => &mut self.view.control.session,
                    ControlId::Workspace => &mut self.view.control.workspace,
                    ControlId::Autonomy => &mut self.view.control.autonomy,
                    ControlId::Provider => &mut self.view.control.provider,
                };
                // An out-of-range option is a named refusal, not a clamp. A clamp would silently
                // select a neighbour and report success, which is the shape of every default this
                // project has had to go back and delete.
                if option >= p.options.len() {
                    return Err(marlowe_view::IntentError::NoSuchOption {
                        control,
                        given: option.to_string(),
                    });
                }
                p.selected = option;
            }
        }
        Ok(())
    }

    fn tick(&mut self, now_ms: u64) {
        Session::tick(self, now_ms)
    }

    fn is_busy(&self) -> bool {
        Session::is_busy(self)
    }

    fn next_beat_in(&self, now_ms: u64) -> Option<u64> {
        Session::next_beat_in(self, now_ms)
    }
}

impl marlowe_view::ClockRead for crate::frame_clock::Clock {
    fn now_ms(&self) -> u64 {
        crate::frame_clock::Clock::now_ms(self)
    }
}

/// The mockup's transcript. Real history, because an empty pane proves nothing.
fn opening_transcript() -> Vec<Entry> {
    vec![
        Entry::Compacted { turns: 47 },
        Entry::User("what does my day look like".into()),
        Entry::Said(Speech::Model(
            "Three things need you. The vendor call at eleven is the one to look at — you owe them \
             a pricing sheet from March and it was never sent."
                .into(),
        )),
        Entry::Tools(vec![
            ToolCall::ok(1, "calendar", "thursday", vec![Metric::Count { n: 4, unit: "events" }]),
            ToolCall::ok(2, "recall", "open commitments", vec![Metric::Count { n: 3, unit: "due" }]),
        ]),
        Entry::User("draft the pricing sheet email".into()),
        Entry::Said(Speech::Model(
            "Drafted, in your register — short, no apology for the delay since you never promised \
             a date. Waiting on you to send."
                .into(),
        )),
        Entry::User("what did we agree with them back in march".into()),
        Entry::Said(Speech::Model(
            "You quoted tiered pricing at three volume bands and said you'd follow up with the \
             sheet inside a week. They accepted the bands verbally. Nothing was signed."
                .into(),
        )),
        Entry::Tools(vec![
            ToolCall::ok(
                3,
                "recall",
                "acme · march · pricing",
                vec![Metric::Count { n: 6, unit: "memories" }],
            ),
            ToolCall::ok(
                4,
                "read",
                "notes/acme-call-0314.md",
                vec![Metric::Count { n: 41, unit: "lines" }],
            ),
        ]),
        Entry::User("did anything change since".into()),
        Entry::Said(Speech::Model(
            "Your cost base moved. The middle band was priced against the old egress rate and that \
             went up eleven percent in June — the sheet as drafted would lose money at volume."
                .into(),
        )),
        Entry::User("flag that in the draft".into()),
        Entry::Said(Speech::Model(
            "Flagged. I've left the bands as agreed and added a line saying the middle band is \
             under review, so you're not reopening the negotiation by email."
                .into(),
        )),
        Entry::Tools(vec![ToolCall::ok(
            5,
            "edit",
            "drafts/acme-pricing.md",
            vec![Metric::Diff { added: 3, removed: 0 }],
        )]),
        Entry::User("also start a deep dive on their pricing model".into()),
        Entry::Said(Speech::Model("Running. About twenty minutes and roughly $3 — I'll ping you.".into())),
        Entry::Tools(vec![ToolCall::ok(
            6,
            "run",
            "deep-research",
            vec![Metric::State("spawned")],
        )]),
    ]
}

/// Scripted replies. Matched on a substring, so a demo can be driven by typing.
///
/// The fallback is **not** a generic acknowledgement. A stub that answers everything with "Got it"
/// would make the conversation pane look like it works while proving nothing about how real prose
/// wraps, and it would violate §C1's verbosity rule in the one file where the persona is easiest
/// to check.
fn reply_for(text: &str, id: u64) -> Vec<Beat> {
    let lower = text.to_lowercase();

    if lower.contains("send") && lower.contains("email") {
        return vec![
            Beat { at_ms: 0, action: Action::Status(StatusState::Thinking, "reading the draft · 1 recipient", &["opus-5 · work", "$0.02 this turn"]) },
            Beat { at_ms: 220, action: Action::StartTools },
            Beat { at_ms: 240, action: Action::Tool(ToolCall::ok(id, "read", "drafts/acme-pricing.md", vec![Metric::Count { n: 34, unit: "lines" }])) },
            Beat { at_ms: 620, action: Action::Say("It's ready. Sending under your name is at its ceiling, so this one needs you.") },
            Beat { at_ms: 900, action: Action::Approval(send_as_you()) },
        ];
    }

    if lower.contains("test") || lower.contains("pytest") {
        return vec![
            Beat { at_ms: 0, action: Action::Status(StatusState::Running, "bash · pytest tests/memory", &["0m 00s elapsed", "opus-5 · work"]) },
            Beat { at_ms: 150, action: Action::StartTools },
            Beat { at_ms: 180, action: Action::Tool(ToolCall::running(id, "bash", "pytest tests/memory", 0)) },
            Beat { at_ms: 2_400, action: Action::Finish(id, ToolLineState::Failed(ResultSummary::with_detail(
                vec![Metric::Test { passed: 9, failed: 3, decis: 14 }],
                "tests/memory/test_decay.py::test_half_life\n    assert 0.51 == approx(0.50, rel=1e-3)\n    E   assert 0.51 == 0.5 ± 5.0e-04",
            ))) },
            Beat { at_ms: 2_500, action: Action::Say("Three failures, all in decay. The half-life assertion is off by a hundredth — that's the 6h window you widened on Tuesday, not a regression in the code.") },
            Beat { at_ms: 2_600, action: Action::Status(StatusState::Idle, "nothing running · 2 background runs elsewhere", &["daemon 6d 4h", "$6.31 today"]) },
        ];
    }

    if lower.contains("read") || lower.contains("look at") || lower.contains("check") {
        // Six reads, so the same-verb collapse is demonstrable by typing rather than by fixture.
        let mut beats = vec![
            Beat { at_ms: 0, action: Action::Status(StatusState::Thinking, "reading 6 files · 0 pending", &["opus-5 · work", "$0.04 this turn"]) },
            Beat { at_ms: 120, action: Action::StartTools },
        ];
        for (i, f) in [
            "src/retrieval/gate.rs",
            "src/retrieval/fuse.rs",
            "src/retrieval/rerank.rs",
            "src/retrieval/lexical.rs",
            "src/retrieval/dense.rs",
            "src/retrieval/mod.rs",
        ]
        .iter()
        .enumerate()
        {
            beats.push(Beat {
                at_ms: 140 + (i as u64) * 60,
                action: Action::Tool(ToolCall::ok(
                    id + i as u64,
                    "read",
                    f,
                    vec![Metric::Count { n: 120 + (i as u64) * 37, unit: "lines" }],
                )),
            });
        }
        beats.push(Beat { at_ms: 700, action: Action::Say("The gate is the only one doing anything interesting. Everything else is plumbing around it.") });
        beats.push(Beat { at_ms: 800, action: Action::Status(StatusState::Idle, "nothing running · 2 background runs elsewhere", &["daemon 6d 4h", "$6.31 today"]) });
        return beats;
    }

    if lower.contains("compact") {
        return vec![Beat { at_ms: 100, action: Action::Compact(31) }];
    }

    if lower.contains("degrade") || lower.contains("offline") {
        return vec![Beat { at_ms: 100, action: Action::Degrade(DegradedPath::DenseRetrievalOffline) }];
    }

    // The fallback. Says what it does not know, once, without hedging around it (§C4).
    vec![
        Beat { at_ms: 0, action: Action::Status(StatusState::Thinking, "no model attached · scripted stub", &["M1 · no network", "$0.00 this turn"]) },
        Beat { at_ms: 400, action: Action::Say("There's no model behind this yet — M1 is the terminal, and the agent lands in M2. Try: send the email, run the tests, read the retrieval code, compact, or degrade.") },
        Beat { at_ms: 500, action: Action::Status(StatusState::Idle, "nothing running · 2 background runs elsewhere", &["daemon 6d 4h", "$6.31 today"]) },
    ]
}

/// §B9's worked example: the irreversible, ceiling-tier case.
///
/// **Every field is now a fact rather than a sentence.** That is the change that matters, and it
/// is the same shape a daemon populates from the permission layer at M2's approval wiring. The
/// novelty judgment is about history and the ceiling is the trust ledger's — neither is knowledge a
/// surface has, so M1's hand-written strings were standing in for a producer that did not exist.
fn send_as_you() -> BlastRadius {
    BlastRadius {
        effect: Effect::Send {
            medium: Medium::Email,
            recipient: Echo::new("mara@acme.com"),
            impersonating: true,
        },
        tier: RiskTier::Irreversible,
        novelty: Novelty::FirstTime(FirstTime::Recipient),
        ceiling: Ceiling::AtCeiling,
        // Addendum A §A3's delegation escape hatch, offered because this producer can "perform"
        // it. A producer that cannot must withhold it rather than show a key that does nothing.
        offered: Offered { edit_first: true, send_as_marlowe: true },
    }
}

fn runs_pane() -> Vec<Item> {
    vec![
        Item::new(
            "deep-research — acme pricing",
            'r',
            Tone::Accent,
            &[("● running · 34 sources · 12 pending", Tone::Normal)],
        ),
        Item::new("Elapsed", 'e', Tone::Normal, &[("4m 12s", Tone::Normal)]),
        // Not 'y': §B10 reserves y/Y for copy, and `KeyRegistry::build` refuses a session that
        // claims them. 'd' for the dollar figure — 's' and 'p' are already region hotkeys.
        Item::new("Spend", 'd', Tone::Amber, &[("$1.20 / $3.00", Tone::Amber)]),
        Item::new("Depth", 'h', Tone::Normal, &[("2 subagents", Tone::Normal)]),
        Item::editable("Steer", 'g', "inject guidance without restarting…"),
        Item::new(
            "test-suite",
            't',
            Tone::Normal,
            &[("● running · 3 of 12 passed", Tone::Normal)],
        ),
        Item::new(
            "pdf-export",
            'x',
            Tone::Dim,
            &[("○ queued · waiting on deep-research", Tone::Dim)],
        ),
        Item::new(
            "Completed today",
            'o',
            Tone::Dim,
            &[("7 runs · $4.80", Tone::Dim)],
        ),
        Item::new("Failed", 'f', Tone::Dim, &[("1 · budget ceiling", Tone::Dim)]),
    ]
}

fn schedule_pane() -> Vec<Item> {
    vec![
        // §B7: events needing nothing are dimmed to near-invisible so the eye goes to the two
        // that do. The dimming is the information.
        Item::new(
            "09:30 · standup",
            'q',
            Tone::Dim,
            &[("15m · no prep needed", Tone::Dim)],
        ),
        Item::new(
            "11:00 · vendor call — acme",
            'e',
            Tone::Accent,
            &[
                ("you owe them the pricing sheet · promised 14 March", Tone::Amber),
                ("last spoke 6 weeks ago · your norm is 2", Tone::Dim),
            ],
        ),
        Item::new(
            "14:00 · design review",
            'r',
            Tone::Red,
            &[
                ("conflict · your flight lands 14:50", Tone::Red),
                ("▸ move to 16:00 · ▸ send regrets", Tone::Dim),
            ],
        ),
        Item::new(
            "18:30 · dinner — mara",
            'n',
            Tone::Dim,
            &[("personal profile · no prep", Tone::Dim)],
        ),
        // A commitment has a deadline and no time slot, so it is a separate region from the
        // events (§B7).
        Item::new(
            "Due today",
            'd',
            Tone::Amber,
            &[
                ("pricing sheet → acme · drafted, unsent", Tone::Amber),
                ("contract review → legal · 8 days late", Tone::Amber),
                ("expenses → finance · draft ready", Tone::Dim),
            ],
        ),
        Item::new("Owed to you", 'o', Tone::Dim, &[("2 · oldest 8 days", Tone::Dim)]),
        Item::new("Tomorrow", 'j', Tone::Dim, &[("1 event · 09:00", Tone::Dim)]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_holds_the_last_meter_frame_rather_than_clearing_it() {
        // C2d changed this test's shape, and the change is the point. M1 asserted that the
        // published `meter` field still held the moving frame. That conflated two facts — what was
        // last measured, and whether anything is measuring now — into one array, so a producer
        // with no telemetry was indistinguishable from one reporting silence.
        //
        // Now the producer publishes `MeterSource::None`, and the HELD frame is what resolves
        // against it. Both halves are asserted: the source stops, and the picture does not.
        let mut s = Session::new();
        s.tick(300);
        let moving = s.last_meter();
        assert_ne!(moving, BASELINE, "listening should be moving");
        assert!(s.view().meter.is_reporting(), "listening has a source attached");

        s.force_state(StatusState::Waiting, 400);
        s.tick(400);
        assert_eq!(
            s.view().meter,
            MeterSource::None,
            "waiting detaches the source; that is ADR-021's freeze arriving as an absence of data \
             rather than as a branch inside the widget"
        );
        assert_eq!(
            s.view().meter.resolve(moving),
            moving,
            "the frame at the moment sampling stopped must stay on screen; a cleared meter reads \
             as 'nothing here' when the truth is 'nothing will happen until you act'"
        );
        s.tick(9_999);
        assert_eq!(s.view().meter.resolve(moving), moving, "and it must stay held, not decay");
    }

    #[test]
    fn idle_reports_a_flat_frame_and_waiting_reports_no_frame_at_all() {
        // The distinction the M1 shape could not express. `idle` is live and silent; `waiting` is
        // not measuring. §B5 shows the user different things for the two.
        let mut s = Session::new();
        s.force_state(StatusState::Idle, 0);
        s.tick(0);
        assert_eq!(s.view().meter, MeterSource::Reported(BASELINE));
        s.force_state(StatusState::Waiting, 1);
        s.tick(1);
        assert_eq!(s.view().meter, MeterSource::None);
    }

    #[test]
    fn driving_waiting_raises_the_overlay_because_they_are_one_situation() {
        let mut s = Session::new();
        s.force_state(StatusState::Waiting, 0);
        assert!(s.view().approval.is_some());
        s.force_state(StatusState::Idle, 1);
        assert!(s.view().approval.is_none());
    }

    #[test]
    fn every_state_is_reachable_by_cycling() {
        let mut s = Session::new();
        let mut seen = Vec::new();
        for i in 0..7 {
            seen.push(s.view().status.state);
            s.cycle_state(i * 10);
        }
        for want in [
            StatusState::Listening,
            StatusState::Thinking,
            StatusState::Speaking,
            StatusState::Writing,
            StatusState::Running,
            StatusState::Waiting,
            StatusState::Idle,
        ] {
            assert!(seen.contains(&want), "{want:?} unreachable by ^v");
        }
    }

    #[test]
    fn consecutive_same_verb_calls_collapse() {
        let mut s = Session::new();
        s.submit("read the retrieval code", 0);
        s.tick(1_000);
        let Some(Entry::Tools(calls)) = s
            .view()
            .transcript
            .iter()
            .rev()
            .find(|e| matches!(e, Entry::Tools(_)))
        else {
            panic!("no tool group");
        };
        assert_eq!(calls.len(), 1, "six reads should render as one line");
        assert_eq!(calls[0].collapsed.len(), 5);
    }

    #[test]
    fn a_failure_auto_expands_and_never_collapses_into_a_group() {
        let mut s = Session::new();
        s.submit("run the tests", 0);
        s.tick(3_000);
        let Some(Entry::Tools(calls)) = s
            .view()
            .transcript
            .iter()
            .rev()
            .find(|e| matches!(e, Entry::Tools(_)))
        else {
            panic!("no tool group");
        };
        assert!(calls[0].is_failure());
        assert!(calls[0].expanded, "§B6: failures auto-expand");
        assert!(calls[0].collapsed.is_empty());
    }

    #[test]
    fn interrupt_keeps_partial_output() {
        let mut s = Session::new();
        let before = s.view().transcript.len();
        s.submit("run the tests", 0);
        s.tick(200);
        s.interrupt(300);
        assert!(!s.is_busy());
        assert!(
            s.view().transcript.len() > before,
            "§B10: partial output is kept, not rolled back"
        );
    }

    #[test]
    fn a_session_replays_identically_on_a_fixed_schedule() {
        // The property every §B13 flicker measurement rests on.
        let render = || {
            let mut s = Session::new();
            s.submit("run the tests", 0);
            for t in (0..3_000).step_by(50) {
                s.tick(t);
            }
            format!("{:?}{:?}{:?}", s.view().transcript, s.view().status, s.last_meter())
        };
        assert_eq!(render(), render());
    }
}
