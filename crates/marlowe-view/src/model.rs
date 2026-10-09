//! The view models a producer publishes and a surface renders.
//!
//! **This crate is M2 C2d paying the debt the M1 version of this file recorded.** It used to sit
//! in `marlowe-stub` under a header saying *"M2 must pin them … they live here, in the stub, until
//! there is a daemon whose shape they describe."* There is now a daemon (ARCHITECTURE §6), so they
//! live here, and the stub is one producer of them rather than their owner.
//!
//! # Why this is its own crate and not part of the daemon
//!
//! Putting them in `marlowe-daemon` would put `marlowe-loop`, `marlowe-provider` and
//! `marlowe-permission` into the surface's dependency tree, and let a surface construct an
//! `Engine`. That is strictly worse than what M1 had. This crate depends on **nothing** — so it
//! can be shared by a producer and a renderer without either gaining reach into the other.
//!
//! # Nothing here can produce a value
//!
//! There are no constructors that invent state, and **deliberately no `Default`**. A surface that
//! can `SessionView::default()` can fabricate exactly the state ARCHITECTURE §2.14 forbids it from
//! holding, and every test would stay green while it did. The producers are
//! `marlowe-stub::Session::view()` (scripted) and `marlowe-daemon::project` (real).

/// §B5's seven states. One region, always visible, outside every scroll area.
///
/// > **Motion means Marlowe is working. Stillness means the ball is in the user's court.**
///
/// That rule is carried by [`StatusState::samples_amplitude`], not by a colour. `Waiting` stops
/// sampling, so the indicator freezes structurally — see ADR-021. Nothing branches on "is this the
/// waiting state" inside the widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Listening,
    Thinking,
    Speaking,
    Writing,
    Running,
    Waiting,
    Idle,
}

/// Which of the three state colours (or none) a thing carries. §B2: **state colours encode state
/// only, never category.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Healthy, live, running normally.
    Green,
    /// Needs attention, approaching a limit, degraded.
    Amber,
    /// Conflict, failure, irreversible.
    Red,
    /// Focus, labels, hotkeys, structure.
    Accent,
    /// Body text.
    Normal,
    /// Inactive, and **load-bearing**: §B2 — an event needing nothing from the user is dimmed to
    /// near-invisible so the eye goes to the ones that do.
    Dim,
}

impl StatusState {
    pub fn name(self) -> &'static str {
        match self {
            StatusState::Listening => "listening",
            StatusState::Thinking => "thinking",
            StatusState::Speaking => "speaking",
            StatusState::Writing => "writing",
            StatusState::Running => "running",
            StatusState::Waiting => "waiting",
            StatusState::Idle => "idle",
        }
    }

    /// §B5's colour column.
    pub fn tone(self) -> Tone {
        match self {
            StatusState::Listening | StatusState::Speaking => Tone::Green,
            StatusState::Thinking | StatusState::Writing => Tone::Accent,
            StatusState::Running | StatusState::Waiting => Tone::Amber,
            StatusState::Idle => Tone::Dim,
        }
    }

    /// **Whether a sample source is attached at all.**
    ///
    /// `Waiting` returns false and that is the whole of §B5's freeze rule. The meter draws what its
    /// source last reported and holds the last frame when the source stops (ADR-021), so "the
    /// indicator freezes when nothing will happen until the user acts" needs no special case.
    ///
    /// `Idle` still samples — it reports a flat zero baseline, which is a reading, not an absence.
    /// A still-but-live indicator and a frozen one are different claims and the user can tell.
    pub fn samples_amplitude(self) -> bool {
        !matches!(self, StatusState::Waiting)
    }

    /// The message field's placeholder tracks the band (§B8). This is how barge-in is made visible
    /// without a second indicator.
    pub fn placeholder(self) -> &'static str {
        match self {
            StatusState::Listening => "listening — type to take over",
            StatusState::Thinking => "thinking — esc to interrupt",
            StatusState::Speaking => "speaking — type to take over",
            StatusState::Writing => "writing — esc to stop",
            StatusState::Running => "running a tool — esc cancels",
            StatusState::Waiting => "waiting on you",
            StatusState::Idle => "ask me something",
        }
    }
}

/// What the status band shows for the current state. §B5: *"the numbers that matter **for that
/// state**"* — a band that always shows the same four numbers is a status bar, not a status band.
/// **What the turn in flight is costing, while it is still in flight.** §B5's figures for
/// `thinking` and `writing`.
///
/// # TTFT is the first token of ANY kind, and that is not the conventional reading
///
/// A reasoning model emits `thinking` long before it emits `content` — `qwen3.5:9b` produced
/// 2,615 of 2,862 frames with an empty `content` field. Timing to the first *answer* token would
/// therefore report the length of the model's deliberation as latency, which is a measurement of
/// something else entirely. What a person perceives as "it started" is the first token that
/// exists, so that is what this measures.
///
/// # The two numbers are one struct because either one alone is a trap
///
/// This project has the receipts. A CPU `llama-server` beat Ollama on TTFT — **218 ms against
/// 426**, a 2x win — while being **five times worse over a whole turn**, because TTFT is prompt
/// eval and prompt eval is the part a CPU does acceptably (`llamacpp.rs`, `Offload`'s header).
/// A band showing only the latency would have rendered that regression as an improvement, on
/// screen, to the person it was happening to.
///
/// So there is no way to put one on screen without the other: the fields are **private**, the only
/// constructor is [`Cadence::new`], and the only renderer is [`Cadence::figures`], which emits both
/// lines or does not exist. A surface cannot reach past it, because there is nothing to reach.
///
/// # `new` returns `Option`, and that `None` is the honest reading of an undefined rate
///
/// At the instant the first token lands there is no elapsed generation time to divide by. A rate
/// over a zero denominator is infinity, renders as `inf tok/s`, and reads as a bug in Marlowe. The
/// band shows the state and its detail and no figures until there is time to measure against —
/// which lasts a fraction of a second and is true throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    ttft_ms: u64,
    tokens: u64,
    since_first_ms: u64,
    warm: bool,
}

impl Cadence {
    /// `None` when there is no rate to measure — see the type's header and [`Self::tok_per_s`].
    ///
    /// `tokens` is what the engine streamed, counted at the harness's own sink. Both local engines
    /// emit one delta per token, so this is a count rather than an estimate from characters.
    ///
    /// **Two refusals, and the second was a bug until 2026-08-30.** A zero denominator is the
    /// obvious one. The other is `tokens < 2`: the rate divides by the interval *since the first
    /// token*, and one token spans no such interval — every rate over it is a rate over a sample
    /// of zero. Returning `Some` there produced `0 tok/s` on a one-token turn, which is not a
    /// smaller number than the truth, it is a different kind of statement.
    pub fn new(ttft_ms: u64, tokens: u64, since_first_ms: u64, warm: bool) -> Option<Self> {
        if since_first_ms == 0 || tokens < 2 {
            return None;
        }
        Some(Self { ttft_ms, tokens, since_first_ms, warm })
    }

    pub fn ttft_ms(self) -> u64 {
        self.ttft_ms
    }

    pub fn tokens(self) -> u64 {
        self.tokens
    }

    /// Whether the engine had already answered a turn when this one started.
    ///
    /// **Not a claim about weights.** The harness does not know whether the model was paged in
    /// from disk; what it knows is which turn this is, and the first turn on a freshly started
    /// engine is the one carrying whatever the load cost. So the rendered words are `first turn on
    /// this engine`, which is a fact, rather than `cold`, which would be an inference.
    pub fn warm(self) -> bool {
        self.warm
    }

    /// Generated tokens per second, over the time since the first token.
    ///
    /// **The denominator excludes TTFT deliberately**, because TTFT already reports that interval.
    /// Dividing by the whole turn would fold prompt eval into the rate and make the two figures
    /// report overlapping things — and it is the *generation* rate that separates a GPU from a CPU
    /// (107 against 10 on this machine), which is the separation the pairing exists to preserve.
    ///
    /// # The numerator is `tokens - 1`, and it was `tokens` until 2026-08-30
    ///
    /// **The two halves of that sentence above have to agree, and they did not.** If the
    /// denominator starts *at* the first token, then the first token was produced **before** the
    /// window opens — its cost is TTFT, which is exactly what the paragraph above says. Counting it
    /// in the numerator put it on both sides of the split: its time in `ttft_ms`, its existence in
    /// the rate. What the window actually contains is the `tokens - 1` deltas that arrived during
    /// it.
    ///
    /// **The error is largest where it is most visible.** `CADENCE_EVERY` is 8, so the first figure
    /// a user ever sees reported 8 tokens over the 7 gaps that produced them — **14% high** — then
    /// 16/15, then 24/23, converging to `N/(N-1)`. A long answer was off by a percent; a short one,
    /// which is the kind you can eyeball against a stopwatch, was off by a seventh. Reported by the
    /// human, who was right.
    ///
    /// Not a division-by-zero risk: [`Self::new`] refuses `tokens < 2`, which is the same refusal as
    /// the zero denominator and for the same reason — there is no interval to measure over.
    pub fn tok_per_s(self) -> f64 {
        (self.tokens - 1) as f64 * 1000.0 / self.since_first_ms as f64
    }

    /// §B5's figures column: at most two lines, right-aligned. **Both numbers, always.**
    pub fn figures(self) -> Vec<String> {
        vec![
            format!("ttft {} ms · {:.0} tok/s", self.ttft_ms, self.tok_per_s()),
            format!(
                "{} tokens · {}",
                self.tokens,
                if self.warm { "warm" } else { "first turn on this engine" }
            ),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBand {
    pub state: StatusState,
    /// What it is doing, in specifics. `reading 34 sources · 12 pending`, never `working`.
    pub detail: String,
    /// The numbers for this state, right-aligned, at most two lines.
    pub figures: Vec<String>,
    /// Set when invariant 4 has fired. Renders amber, over the state's own detail.
    pub degraded: Option<crate::turn::DegradedPath>,
    /// **The turn in flight, measured.** `None` between turns, and until the first token lands.
    ///
    /// It is not folded into [`Self::figures`] as two more strings, because the whole point of
    /// [`Cadence`] is that the pair cannot be split — and a `Vec<String>` is precisely the shape
    /// somebody splits. A renderer asks the struct for its lines; it never composes them.
    pub cadence: Option<Cadence>,
}

/// §B4's five control-strip regions, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlStrip {
    pub model: Picker,
    pub profile: Picker,
    pub session: Picker,
    pub workspace: Picker,
    /// **The one that matters.** It is the single control changing what Marlowe may do without
    /// asking, it carries state colour, and it is **never a value the agent can change**
    /// (Addendum A §A8: self-granted promotion is structurally impossible).
    pub autonomy: Picker,
    /// Which provider serves the model — `ollama` or `openrouter`. ADR-046 made this a
    /// launch-time choice; ADR-049 §7 makes it selectable from the session.
    ///
    /// **It is a control and it is deliberately NOT on the strip**, which is why it sits after
    /// `autonomy` rather than among the five. The strip is `[Rect; 5]` and §B13's layout rows are
    /// measured against that width at five sizes; widening it to six is a real interface change
    /// with a real argument, and it is not one to make as a side effect of adding a command.
    /// `ControlId::ALL` therefore still has five entries and this one is reachable by `/provider`.
    ///
    /// The value is not hidden by that: `StatusBand` already announces the provider (ADR-029 —
    /// announced, never inferred), so the state is on screen and only the *control* is by command.
    pub provider: Picker,
}

/// A control-strip value with its in-place selection list. Drawn by Marlowe, never an OS widget.
///
/// # `open` is not here, and that is C2d's correction
///
/// The M1 shape carried `open: bool` with the comment *"open state is the surface's business, but
/// it lives with the value so a redraw cannot lose it."* The first half is right and the second is
/// how it ended up on the wrong side of the boundary — a redraw cannot lose surface state that the
/// surface owns either. `App::picker_open` holds it now.
///
/// `selected`, by contrast, **is** the daemon's: which model, profile, session and workspace are
/// live are facts about the run, not about looking at it. A surface changes one by asking
/// ([`crate::view::Intent`]), never by assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub options: Vec<String>,
    pub selected: usize,
}

impl Picker {
    pub fn new(options: &[&str], selected: usize) -> Self {
        assert!(
            selected < options.len(),
            "Picker::new selected index {selected} is out of range for {} options; a picker with \
             no valid selection would render an empty region with a border and a hotkey, which is \
             a region that lies about being interactive",
            options.len()
        );
        Self {
            options: options.iter().map(|s| (*s).to_string()).collect(),
            selected,
        }
    }

    pub fn value(&self) -> &str {
        &self.options[self.selected]
    }

    /// Autonomy is amber at `confirm` and above (§B4). Every other picker is toneless — a control
    /// strip where four of five fields carry colour is a control strip where colour means nothing.
    pub fn autonomy_tone(&self) -> Tone {
        match self.value() {
            "confirm" | "act" => Tone::Amber,
            _ => Tone::Normal,
        }
    }
}

/// One line in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// Marlowe's prose. Carries the persona (Addendum C).
    ///
    /// **The payload is [`crate::notice::Speech`], not a `String`.** A free string here is what let
    /// M1's surface author Marlowe's voice and would have let any producer do the same; the model
    /// half stays a `String` because model output is one, and the harness half is a closed
    /// vocabulary. ADR-030.
    Said(crate::notice::Speech),
    /// A group of consecutive tool calls. §B6's same-verb collapse operates within a group.
    Tools(Vec<ToolCall>),
    /// The model's reasoning, **collapsed by default**.
    ///
    /// Appears the moment generation starts, so a reasoning model is visibly working rather than
    /// apparently hung — `qwen3.5:9b` spent 91% of its frames here on a one-word greeting, and
    /// none of it was on screen.
    ///
    /// It is its own variant rather than a `Said` because it is **not what Marlowe said**: it does
    /// not carry the persona, it must not appear in a `Y` transcript copy, and the user asked a
    /// question rather than for a monologue. Expansion is the surface's, exactly as for a tool
    /// line — see `App::expanded`.
    ///
    /// # `tokens` is CARRIED, because a renderer cannot compute it
    ///
    /// The collapsed head line reports how much thinking has happened. It reported `text.len()`,
    /// and characters are what a renderer can count for itself — a token count derived from the
    /// same string would be an estimate wearing a unit, and the one thing this line must not do is
    /// look precise while being arithmetic on characters.
    ///
    /// So the number arrives from the provider, which is the only component that can see the
    /// engine's own counter, and is **accumulated** here. Nothing downstream re-derives it. A
    /// block whose text grew without its count growing is a provider reporting no new tokens for
    /// those bytes, and that is a fact about the engine rather than a rounding error to paper over.
    Reasoning { text: String, tokens: u64, done: bool },
    /// `─ compacted · 47 turns → summary ─`. Announces itself inline and does not interrupt.
    Compacted { turns: u32 },
}

/// One rendered tool call. §B6: **one line**, expandable, failures auto-expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: u64,
    pub verb: &'static str,
    pub target: String,
    pub state: crate::turn::ToolLineState,
    /// Set when consecutive same-verb calls collapsed into this one: six reads become
    /// `⋯ read  6 files`. The collapsed targets are kept so expanding shows what they were.
    pub collapsed: Vec<String>,
    /// User expanded it with Enter/Tab. Failures start expanded and this is why the field is not
    /// simply derived from the state — a user can also collapse a failure back down.
    pub expanded: bool,
    /// §B6's right-hand side when it arrived **already rendered**, off the wire.
    ///
    /// # Why this is not `state`'s metrics
    ///
    /// [`crate::turn::Metric`] is typed and every variant is `&'static str` or a number — by
    /// construction, so §B6's vocabulary cannot become a free-text escape hatch. A summary that
    /// crossed a socket is a `String`, and parsing it back into metrics would be a second
    /// definition of the summary grammar that goes wrong the first time a metric changes.
    ///
    /// So the daemon's folds could not put `48 lines` where it belongs, and put
    /// `Metric::State("ok")` there instead — the §B6 line read **`ok`** on every successful call,
    /// with the real summary buried in the expansion, which is the field the tool's own output now
    /// needs. Two things were in one slot; this is the second slot.
    ///
    /// `None` means "this producer built real metrics" — the stub, and every in-process view.
    /// `Some` means "already rendered upstream, render it verbatim". A renderer must prefer this
    /// when it is set.
    pub summary_line: Option<String>,
}

impl ToolCall {
    pub fn ok(id: u64, verb: &'static str, target: &str, metrics: Vec<crate::turn::Metric>) -> Self {
        Self {
            id,
            verb,
            target: target.to_string(),
            state: crate::turn::ToolLineState::Ok(crate::turn::ResultSummary::new(metrics)),
            collapsed: Vec::new(),
            summary_line: None,
            expanded: false,
        }
    }

    pub fn failed(
        id: u64,
        verb: &'static str,
        target: &str,
        metrics: Vec<crate::turn::Metric>,
        detail: &str,
    ) -> Self {
        Self {
            id,
            verb,
            target: target.to_string(),
            state: crate::turn::ToolLineState::Failed(crate::turn::ResultSummary::with_detail(
                metrics, detail,
            )),
            collapsed: Vec::new(),
            summary_line: None,
            expanded: true, // §B6: failures auto-expand.
        }
    }

    pub fn running(id: u64, verb: &'static str, target: &str, elapsed_ms: u64) -> Self {
        Self {
            id,
            verb,
            target: target.to_string(),
            state: crate::turn::ToolLineState::Running { elapsed_ms },
            collapsed: Vec::new(),
            summary_line: None,
            expanded: false,
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self.state, crate::turn::ToolLineState::Failed(_))
    }
}

/// The pinned pager (§B6). Carries what no other region has, and it never scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pager {
    pub turn: u32,
    pub compacted: u32,
    /// How many compaction generations this session goes back. What `^l` walks.
    pub lineage: u32,
}

/// The six inspector tabs (§B7). Tabs are pinned; content scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Runs,
    Schedule,
    Sessions,
    Skills,
    Trust,
    Status,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Runs,
        Tab::Schedule,
        Tab::Sessions,
        Tab::Skills,
        Tab::Trust,
        Tab::Status,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Runs => "Runs",
            Tab::Schedule => "Schedule",
            Tab::Sessions => "Sessions",
            Tab::Skills => "Skills",
            Tab::Trust => "Trust",
            Tab::Status => "Status",
        }
    }

    /// The digit that reaches this tab. `1`–`6`, per §B7.
    pub fn digit(self) -> char {
        match self {
            Tab::Runs => '1',
            Tab::Schedule => '2',
            Tab::Sessions => '3',
            Tab::Skills => '4',
            Tab::Trust => '5',
            Tab::Status => '6',
        }
    }

    /// Whether M1 renders this tab with data.
    ///
    /// **The four that are not live are still present and still reachable**, each rendering one
    /// bordered region that says what will live there and in which milestone. §B7's tab bar must
    /// not lie — a tab that silently shows nothing is worse than a tab that says it is not built,
    /// because the user cannot tell it apart from a bug.
    pub fn is_live_in_m1(self) -> bool {
        matches!(self, Tab::Runs | Tab::Schedule)
    }
}

/// An item inside an inspector pane.
///
/// §B7: *"Each item inside is itself a bordered region with a label and a key."* The mockup shows
/// several bordered items with no key; prose wins, and the surface's `Region` type makes a keyless
/// bordered region unrepresentable anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// What this item **is**, when the label is a rendering of something rather than the thing.
    ///
    /// A run's label is its mnemonic — `daring-storm` — and its identity is the UUID the journal
    /// keys on. They are deliberately two fields: 4096 names means two live runs can share one,
    /// so folding an updated run onto its row by name could merge two runs into one. `None` for
    /// every item whose label is all there is.
    pub id: Option<String>,
    /// The key on this item's bottom border, when there is one.
    ///
    /// # `None` is a pane that ran out of letters, and it is better than the alternative
    ///
    /// Registered in the surface's key registry, which **errors on a collision** rather than
    /// silently letting one key shadow another. The pane's pool is seventeen letters — every other
    /// lowercase key is already spoken for by the region keys, the copy keys and the tab digits —
    /// and `pane_key` used to CLAMP past the end, handing every later item the same `z`.
    ///
    /// That was invisible while nothing rebuilt the registry. Once `/runs` went live it became far
    /// worse than "the eighteenth run has no key": the rebuild hit the collision, refused, and the
    /// surface kept the previous registry — so **no run key worked at all** past seventeen runs.
    ///
    /// An item with no key is still a region: it has a border, it takes focus, and the arrows, the
    /// wheel and the pointer all reach it. What it does not have is an accelerator, which is the
    /// honest thing to render when there is no letter left to give.
    pub key: Option<char>,
    /// Lines inside the region, each with its tone. Dimming is how a dense screen tells the eye
    /// where to look, so this is data, not styling.
    pub lines: Vec<(String, Tone)>,
    /// The border tone. `Dim` for an item needing nothing from the user.
    pub tone: Tone,
    /// A text field the user types into — the Steer field is the only one in M1 (§B7, v1.0 §10.1:
    /// *inject guidance into a running child without restarting it*).
    pub editable: bool,
}

impl Item {
    pub fn new(label: &str, key: char, tone: Tone, lines: &[(&str, Tone)]) -> Self {
        Self {
            label: label.to_string(),
            id: None,
            key: Some(key),
            lines: lines.iter().map(|(s, t)| ((*s).to_string(), *t)).collect(),
            tone,
            editable: false,
        }
    }

    /// An item with no accelerator. See [`Item::key`] for when that happens and why it beats
    /// handing two items the same letter.
    pub fn unkeyed(label: &str, tone: Tone, lines: &[(&str, Tone)]) -> Self {
        Self { key: None, ..Self::new(label, 'a', tone, lines) }
    }

    /// Name what this item is, when its label is a rendering of that rather than the thing itself.
    pub fn identified(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn editable(label: &str, key: char, placeholder: &str) -> Self {
        Self {
            label: label.to_string(),
            id: None,
            key: Some(key),
            lines: vec![(placeholder.to_string(), Tone::Dim)],
            tone: Tone::Normal,
            editable: true,
        }
    }
}

/// The ambient numbers on the message field's right (§B8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ambient {
    pub fill_pct: u8,
    pub spend_cents: u32,
    pub elapsed_min: u32,
}

impl Ambient {
    /// **Context pressure is a colour, not a bar** (§B8): dim until 60%, amber at 60%, accent at
    /// the 70% compaction trigger (v1.0 §6).
    pub fn context_tone(&self) -> Tone {
        match self.fill_pct {
            0..=59 => Tone::Dim,
            60..=69 => Tone::Amber,
            _ => Tone::Accent,
        }
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::Cadence;

    /// **The off-by-one, stated as the arithmetic a human can check by hand.**
    ///
    /// Eight tokens, and the first of them arrived at `t=0` of this window — that is what
    /// "since first" means. So seven deltas landed across 700 ms and the rate is 10/s, not the
    /// 11.43/s the old numerator produced. `CADENCE_EVERY` is 8, so this is the *first* figure a
    /// user ever sees on a turn.
    #[test]
    fn the_rate_counts_the_gaps_between_tokens_not_the_tokens() {
        let c = Cadence::new(300, 8, 700, true).expect("two tokens and elapsed time is a rate");
        assert!(
            (c.tok_per_s() - 10.0).abs() < 1e-9,
            "8 tokens whose first opened the window is 7 deltas in 700 ms = 10 tok/s; got {} \
             (11.43 means the numerator counted the first token, whose time is in ttft)",
            c.tok_per_s()
        );
    }

    /// The anti-vacuity half: a *different* count must produce a *different* rate. Without this,
    /// a `tok_per_s` hardcoded to 10.0 passes the test above.
    #[test]
    fn the_rate_moves_with_the_count() {
        let slow = Cadence::new(300, 8, 700, true).unwrap();
        let fast = Cadence::new(300, 15, 700, true).unwrap();
        assert!(
            fast.tok_per_s() > slow.tok_per_s() * 1.9,
            "fourteen deltas in the same window must be about twice seven: {} vs {}",
            fast.tok_per_s(),
            slow.tok_per_s()
        );
    }

    /// A single token spans no interval, so there is no rate — the same refusal as a zero
    /// denominator. **`Some` here would render `0 tok/s`, which is a claim rather than a gap.**
    #[test]
    fn one_token_is_not_a_rate() {
        assert!(Cadence::new(300, 1, 700, true).is_none(), "one token spans no interval");
        assert!(Cadence::new(300, 0, 700, true).is_none(), "no tokens is not a rate either");
        assert!(Cadence::new(300, 8, 0, true).is_none(), "and neither is a zero denominator");
    }

    /// §B5: both numbers or neither. The pairing is the point — a band showing only latency
    /// rendered a 5x whole-turn regression as a 2x improvement, which is why the fields are
    /// private and `figures` is the only renderer.
    #[test]
    fn figures_always_carries_the_rate_beside_the_latency() {
        let f = Cadence::new(300, 8, 700, true).unwrap().figures();
        assert!(f[0].contains("ttft 300 ms"), "the latency half: {:?}", f);
        assert!(f[0].contains("10 tok/s"), "the rate half, rounded: {:?}", f);
        assert!(f[1].contains("8 tokens"), "the count is reported whole, not as gaps: {:?}", f);
    }

    /// The count on screen is the tokens the engine sent. Only the *rate* divides by gaps, and
    /// conflating the two would make the band disagree with itself.
    #[test]
    fn the_reported_count_is_not_reduced_by_one() {
        assert_eq!(Cadence::new(300, 8, 700, true).unwrap().tokens(), 8);
    }
}
