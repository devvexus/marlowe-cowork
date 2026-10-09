//! Focus, input, and key dispatch. **Built before any content, per §B10.**
//!
//! > Keyboard first. The mockup is mouse-driven; the implementation must not be. Build every path
//! > by key and add mouse as a bonus. Mouse-first retrofitted with keys produces a bad TUI.
//!
//! Nothing in this module draws. That is deliberate: every navigation property §B13 asks for is
//! testable here without a terminal, and a key path that only works when something is on screen is
//! a key path that breaks the first time the screen changes.
//!
//! # `Esc` backs out one level, and interrupt is the outermost level
//!
//! §B10 says both *"`Esc` backs out one level"* and *"`Esc` interrupts"*. They are the same rule
//! applied at different depths, and the precedence is fixed here rather than left to whichever
//! branch runs first:
//!
//! 1. approval overlay up → **deny** (§B9)
//! 2. a control-strip dropdown open → close it
//! 3. focus is in a text field → blur to the conversation
//! 4. otherwise → **interrupt**, keeping partial output (§B10)
//!
//! # The one place region hotkeys are suspended
//!
//! While a text field has focus — the Message field, or the Runs pane's Steer field — a plain
//! character is text. It has to be: a message you cannot type the letter `m` into is not a message
//! field.
//!
//! Every region is still reachable by keyboard alone from there, three ways: `Tab`/`Shift-Tab`
//! cycle, `Esc` blurs to the conversation where every hotkey is live, and the Ctrl-modified footer
//! keys never suspend. `tests/keyboard_reachability.rs` proves it from the default focus rather
//! than from a convenient one.

use marlowe_view::notice::{Capability, Milestone, Notice, Refusal};
use marlowe_view::{ClientLine, ControlId, Echo, Intent, PendingLine, SessionView, StatusState, Tab, Tone};

use crate::commands::{self, Outcome};
use crate::keys::{Binding, KeyRegistry};
use crate::region::{RegionId, RegionTree, TabId};

/// §B8's footer: **one line of global keys, always the same, never context-dependent.** Region
/// hotkeys live on their own borders; the footer is for actions.
pub const FOOTER_KEYS: &[(&str, &str)] = &[
    // ADR-056: `Ctrl-V` is paste. Written `alt-v` rather than `⌥v`, which is a Mac glyph on a
    // platform whose key is labelled Alt.
    ("alt-v", "Voice"),
    ("^n", "New"),
    ("^r", "Runs"),
    ("^l", "Lineage"),
    ("^t", "Trust"),
    ("^u", "Undo"),
    ("esc", "Interrupt"),
    ("^k", "palette"),
    // **Advertised, at last.** It has quit the application since M1 and appeared on no footer and
    // in no test. A key that ends the session is the last one that should be folklore.
    ("^c", "copy / quit"),
];

/// A key, decoupled from crossterm so the whole dispatch table is testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    /// The footer's **second** modifier namespace, introduced by ADR-056 so `Ctrl-V` could be
    /// given to paste.
    ///
    /// It holds exactly one binding — `Alt-V`, §B5's state cycle — and is deliberately not open
    /// season: §B10's footer is still `Ctrl`-first. See the ADR for why `Alt` rather than a worse
    /// `Ctrl` letter, and for the reliability cost that is spent on a demo affordance precisely
    /// because no capability depends on it.
    Alt(char),
    Enter,
    ShiftEnter,
    Tab,
    BackTab,
    Backspace,
    /// **Delete the previous word.** Its own variant rather than `Ctrl('\u{8}')`, because three
    /// different physical chords produce it and they must not each grow a handler:
    ///
    /// | chord | who sends it |
    /// |---|---|
    /// | `Ctrl-Backspace` | Windows consoles, where crossterm reads the modifier from the console API |
    /// | `Ctrl-W` | every readline shell, and the one muscle memory can be relied on |
    /// | `Ctrl-H` | what many Unix terminals actually emit for `Ctrl-Backspace` |
    ///
    /// Collapsing them at [`crate::app::Key`] rather than at the handler also keeps them out of
    /// `on_global_ctrl`, where `Ctrl-W` would otherwise have to be reserved against a future
    /// footer key.
    CtrlBackspace,
    Up,
    Down,
    Left,
    Right,
    Esc,
}

/// What the caller should do after a key. The app never exits or draws on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
}

/// The surface's own state — and **only** the surface's own state.
///
/// Focus, scroll offsets, what the user has typed, which lines are expanded. Every one of those is
/// a property of *looking at* the session, not of the session. ARCHITECTURE.md §2.14: a surface
/// holds no state the daemon does not have, and closing it does not affect a run.
#[derive(Debug)]
pub struct App {
    /// **The producer's view, read-only.** Replaced wholesale by [`App::update`]; never edited.
    ///
    /// Private, and that is the C2d change with the most consequence. It was `pub session: Session`
    /// and the surface pushed into its transcript — see `marlowe_view::view`'s header.
    view: SessionView,
    /// What the user has typed and the producer has not confirmed. Rendered as pending, **never
    /// merged into the transcript**; retired only by the producer acknowledging it.
    pub pending: Option<PendingLine>,
    /// Command output, rejections and notices. **The client talking, not Marlowe** — these are not
    /// conversation and never enter the transcript or a `Y` copy.
    pub client_lines: Vec<ClientLine>,
    /// The last `/doctor` report. Diagnostic output, deliberately outside the Notice vocabulary.
    pub diagnostic_lines: Vec<String>,
    /// Requests waiting for the driver to hand to a producer. Drained by [`App::drain_intents`].
    outbox: Vec<Intent>,
    /// Blocks pasted into the composer, in the order they were pasted. The composer holds a chip
    /// for each; [`App::expand_pastes`] puts the text back when the message is sent. See
    /// [`crate::commands::paste_marker`].
    pastes: Vec<String>,
    /// Whether `Ctrl-A` has selected the whole composer.
    ///
    /// **A flag and not a range**, because there is no caret to anchor a range to: this composer
    /// appends and deletes at the end, so "all of it" is the only selection that means anything
    /// yet. The next printable key replaces the buffer, `Backspace` empties it, and `Esc` lets go.
    ///
    /// **It renders as a colour, never a fill.** §B2's *"never a background fill"* is asserted by
    /// `no_background_fill.rs` over every cell, so the usual reverse-video selection is not
    /// available. Selected text takes the accent, which is inside the palette and inside the
    /// budget.
    select_all: bool,
    /// How far the composer is scrolled **back from its end**, in wrapped rows.
    ///
    /// `0` is pinned to the last line, which is where typing belongs. Anything else is a position
    /// the user chose with `Up`, and typing puts it back — a composer that stayed scrolled back
    /// while characters landed off-screen would be worse than one that never moved.
    ///
    /// **Distance from the bottom, not an absolute offset**, so nothing here needs the wrapped
    /// line count: [`crate::render::draw`] clamps against the height it is actually given. The run
    /// window needs a `scroll_max_hint` because a key handler reads that value; this one is read
    /// only by the draw, so it carries no cross-frame dependency.
    composer_scroll: u16,
    /// Whether one `Ctrl-C` has already been pressed with nothing to copy.
    ///
    /// # Why quitting needs two presses now
    ///
    /// `Ctrl-C` was `Action::Quit`: immediate, and it discarded whatever was in the composer. It
    /// was also **not in [`FOOTER_KEYS`]** and **asserted by no test** — a quit path that was
    /// neither advertised nor covered.
    ///
    /// What made it urgent is that `Ctrl-A` arrived in the same session. The keystroke everyone
    /// reaches for immediately after select-all is `Ctrl-C`, so adding one made the other's worst
    /// case far more likely: a person selecting their draft in order to copy it, and losing it
    /// instead.
    ///
    /// **Disarmed by any other key**, so the two presses have to be consecutive. An arming flag
    /// that survived a keystroke would turn a `Ctrl-C` from five minutes ago into half of a quit.
    quit_armed: bool,
    /// A pane that was opened with [`crate::commands::Outcome::TabLive`] and is waiting for the
    /// producer's answer before it says how many of anything it holds.
    ///
    /// **The summary is a count, and a count composed before the answer is the stale reading
    /// `/runs` shipped with.** `advance` applies intents and republishes in one crank, so this is
    /// held for exactly one [`App::update`].
    pending_pane_summary: Option<marlowe_view::Tab>,
    /// `Some` while the user is typing a reason for declining. **Only reachable from the live
    /// approval window**, and leaving it does not answer — `Esc` returns to the three keys rather
    /// than dismissing the question, because a dismissed question is a hung turn.
    pub decline_reason: Option<String>,
    /// Which control-strip dropdown is open. Was `Picker::open` on the producer's side; open-ness
    /// is a property of looking at a control, not of the control.
    pub picker_open: Option<ControlId>,
    /// The highlighted option inside an open dropdown.
    ///
    /// **Surface state, and M1 did not have it** — the highlight *was* `Picker::selected`, so
    /// arrowing through the Autonomy list granted each tier in passing. See `on_picker_key`.
    pub picker_cursor: usize,
    /// Whether thinking blocks are expanded. **Surface state**: collapsed by default, because the
    /// user asked a question rather than for a monologue.
    ///
    /// One flag for all of them rather than one per block, and that is a real limitation stated
    /// rather than hidden: §B6 defers per-line cursoring in the conversation to M2 with real
    /// scrollback, so there is nothing to address an individual block *with*. Expand-all is the
    /// honest affordance until there is a cursor; a per-block flag with no way to point at a block
    /// would be state the user cannot reach.
    pub reasoning_expanded: bool,
    /// The last frame an amplitude source actually reported.
    ///
    /// ADR-021's freeze needs somewhere to hold, and it has to be on the *looking* side: the
    /// producer publishes `MeterSource::None` when nothing is measuring, and this is what that
    /// resolves against. Keeping it here rather than in the view is what lets a producer say "I am
    /// not measuring" without also having to remember what the last picture was.
    last_meter: marlowe_view::Frame,
    /// Tool lines the user has toggled, by call id.
    ///
    /// `ToolCall::expanded` is the producer's default (§B6: failures auto-expand). Whether *you*
    /// then opened or closed one is a property of looking at it, so the override lives here and
    /// the producer's value is what it falls back to.
    pub expanded: std::collections::BTreeMap<u64, bool>,
    /// The inspector tab being shown. Was `Session::tab`, for the same reason.
    pub tab: Tab,
    pub focus: RegionId,
    pub input: String,
    pub steer: String,
    /// **What the DRIVER must act on**, drained by it each pass. `M3-DESIGN.md` §6.
    ///
    /// Opening a window is a process spawn and steering is a socket write, and a surface holds
    /// neither — `marlowe-surface` depends on `marlowe-view` and `ratatui` and nothing that could
    /// do either (see this crate's header, and the `Cargo.toml` note that makes it structural).
    ///
    /// The same shape as `drain_intents`: the surface says what was asked for, something else does
    /// it, and the confirmation is a line the driver adds when it knows the answer.
    window_asks: Vec<crate::commands::Outcome>,
    /// Transcript scroll offset in lines from the top. `None` means pinned to the bottom, which is
    /// what a live conversation wants and is not the same as "offset happens to be the maximum".
    pub scroll: Option<u16>,
    pub inspector_scroll: u16,
    /// Command output and notices, appended to the transcript as Marlowe's own lines.
    pub keys: KeyRegistry,
    /// Set when the terminal is too small. §B11: an honest refusal, never a degraded grid.
    pub too_small: Option<(u16, u16)>,

    /// **On-screen ground truth for a live session.**
    ///
    /// Everything about a running TUI that matters — is raw mode on, is the colour tier what the
    /// probe claimed, are keystrokes arriving — is invisible from outside the process. Inferring
    /// it from a separate `--timing-probe` run measures a *different* process, and inferring it
    /// from a screenshot measures a guess.
    ///
    /// So the program says it, in the frame, under `--diagnostic`. One picture then answers all
    /// three questions at once. This exists because a session that renders, animates, and ignores
    /// every key looks exactly like a session that works.
    pub diagnostic: Option<String>,
    /// Keystrokes that reached [`App::on_key`]. Rendered by `--diagnostic`.
    pub keys_seen: u32,
    /// The last key that arrived, as the app saw it.
    pub last_key: String,
    /// The largest scroll offset the last drawn frame allowed, written by `render::draw`.
    ///
    /// A `Cell` because it is a *measurement of the last frame*, not state the user changed —
    /// `draw` takes `&App` precisely so that rendering cannot mutate the session, and that
    /// restriction is worth keeping for everything except this. The alternative was for `App` to
    /// re-wrap the transcript itself to count lines, which means knowing the pane width, which
    /// means the layout leaking into the key dispatch.
    ///
    /// Zero until the first frame is drawn: scrolling before anything has been rendered is not a
    /// meaningful request, and treating it as "nothing to scroll" is correct rather than defensive.
    pub scroll_max: std::cell::Cell<u16>,
    /// The furthest the inspector pane can scroll, and the last item actually drawn — both written
    /// by `render::draw` for the same reason as `scroll_max`: only the renderer knows how many
    /// items fit in the pane at this window height.
    pub inspector_scroll_max: std::cell::Cell<u16>,
    pub inspector_last_visible: std::cell::Cell<u16>,
    /// Text waiting to be written to the clipboard, drained by the event loop.
    ///
    /// The app cannot write to the clipboard itself — that needs stdout, which is the terminal's,
    /// not the surface's. So `y` builds the payload here and the loop performs the OSC 52 write.
    /// Keeping the payload construction on this side is what lets `clipboard.rs` be tested without
    /// a terminal, which matters because the payloads are the part with the logic in them.
    pub pending_copy: Option<String>,
    /// A one-line confirmation shown until the next keystroke.
    ///
    /// **OSC 52 is write-only and unacknowledged**, so nothing downstream can tell the user whether
    /// the clipboard actually took it. The app therefore states what it sent and how much. An
    /// honest "copied 412 characters" beside an empty clipboard is a reportable bug; a silent
    /// no-op is a mystery, and this project has spent enough rounds on those.
    ///
    /// Cleared on the next key rather than on a timer: no clock is read, the surface stays a pure
    /// function of `(state, now_ms)`, and the confirmation lasts exactly as long as the user is
    /// still looking at the thing they just did.
    pub notice: Option<String>,
    /// Which slash-command suggestion is selected while typing one.
    ///
    /// §B10: *"slash commands autocomplete inline with descriptions."* A list you can see but not
    /// choose from is a hint, not a completion — so the arrows move this and `Tab` accepts it,
    /// which is what every shell and editor has trained the hands to expect.
    ///
    /// Reset to 0 on every edit of the input: after typing another character the old index points
    /// into a different list, and silently keeping it is how `/q` + `Down` + `Tab` completes to
    /// something the user never saw highlighted.
    pub completion: usize,
    /// The inspector tab the pointer is over, if any.
    ///
    /// A third hover slot rather than a variant of `hover`, for the same reason as `hover_option`:
    /// tabs are not regions. Each tab carries its own hotkey, so under §B2 the *bar* has no border
    /// and no `RegionId` — which is correct for the region contract and means the mouse needs its
    /// own hit-test here.
    pub hover_tab: Option<marlowe_view::Tab>,
    /// The option index the pointer is over inside an OPEN dropdown, if any.
    ///
    /// Separate from `hover` because a dropdown's options are not regions — they have no border,
    /// no label and no hotkey of their own (§B2), so they cannot be `RegionId`s. Cleared by the
    /// same rules as `hover`, and additionally whenever no dropdown is open.
    pub hover_option: Option<usize>,
    /// The region the pointer is currently over, if any.
    ///
    /// **Deliberately not the conversation.** Hovering a transcript means nothing, and the pointer
    /// crossing it would repaint its border on every motion event — a flicker source with no
    /// information in it. `hoverable` is the whole rule and it lives in one place.
    ///
    /// A stuck hover is worse than no hover, so this clears on two events and not one: a motion
    /// that lands outside every hoverable region, **and** the terminal losing focus. Only the
    /// second covers the pointer leaving the window entirely, because no motion event is delivered
    /// once it does — the app would otherwise keep a highlight lit under a pointer that is now in
    /// another application.
    pub hover: Option<RegionId>,
    /// Mouse events that reached the loop, counted separately from keys.
    ///
    /// **Separate because a shared counter made an observation ambiguous exactly where it needed to
    /// be sharp.** The wheel is dispatched as `Key::Up`/`Key::Down` — deliberately, so the mouse
    /// adds no second scroll path — which means every wheel notch also ticks `keys_seen`. A frame
    /// reading `keys=2 last=Down` on a session where nothing had been typed was then equally
    /// consistent with "two phantom keystrokes" and "the mouse moved", and those call for opposite
    /// investigations. Two counters make the frame say which.
    pub mouse_seen: u32,
}

/// §B11's minimum. A narrow variant was designed and rejected: it cost the borders, which cost the
/// region contract, which is the entire design.
pub const MIN_COLS: u16 = 120;
pub const MIN_ROWS: u16 = 30;

impl App {
    /// Build, validating the key registry. **A conflict is a refusal to start** — see `keys.rs`.
    pub fn new(view: SessionView) -> Result<Self, crate::keys::KeyConflict> {
        let keys = KeyRegistry::build(&view)?;
        Ok(Self {
            decline_reason: None,
            view,
            pending: None,
            client_lines: Vec::new(),
            diagnostic_lines: Vec::new(),
            outbox: Vec::new(),
            pastes: Vec::new(),
            select_all: false,
            composer_scroll: 0,
            quit_armed: false,
            pending_pane_summary: None,
            picker_open: None,
            picker_cursor: 0,
            reasoning_expanded: false,
            last_meter: marlowe_view::BASELINE,
            expanded: std::collections::BTreeMap::new(),
            tab: Tab::Schedule,
            // **The conversation, not the message field.**
            //
            // The first version started in the Message field, on the reasoning that the user came
            // here to type. It made §B10's first sentence false on arrival: *"Region hotkeys jump
            // focus directly — `m` to Model, `2` to Schedule, `c` to the conversation."* From a
            // focused text field `m` is the letter m, so every hotkey printed on every border did
            // nothing until the user guessed `Esc` first. The borders were advertising a contract
            // the default state broke.
            //
            // Starting on the conversation makes every hotkey live immediately, and `i` — already
            // printed on the message field's own bottom border — is how you begin typing. That is
            // the same shape as the rest of the design rather than an exception to it.
            focus: RegionId::Conversation,
            input: String::new(),
            steer: String::new(),
            window_asks: Vec::new(),
            scroll: None,
            inspector_scroll: 0,
            keys,
            too_small: None,
            diagnostic: None,
            keys_seen: 0,
            last_key: String::from("none"),
            mouse_seen: 0,
            scroll_max: std::cell::Cell::new(0),
            inspector_scroll_max: std::cell::Cell::new(0),
            inspector_last_visible: std::cell::Cell::new(0),
            completion: 0,
            hover: None,
            hover_tab: None,
            hover_option: None,
            pending_copy: None,
            notice: None,
        })
    }

    /// The producer's view. Read-only to everything, including the rest of this crate.
    pub fn view(&self) -> &SessionView {
        &self.view
    }

    /// A producer published a new view.
    ///
    /// **This is the only way the view changes, and it is where a pending line is retired.**
    /// Retirement is an observation — the producer's transcript now contains the line — and never
    /// a timeout, because a surface that promoted its own optimistic text after N seconds would
    /// author conversation no journal has a record of.
    pub fn update(&mut self, view: SessionView) {
        if let Some(p) = &self.pending {
            if p.is_acknowledged_by(&view) {
                self.pending = None;
            }
        }
        if let marlowe_view::MeterSource::Reported(f) = view.meter {
            self.last_meter = f;
        }
        // **The registry is rebuilt when the items change, and until M3 F2 it never was.**
        //
        // `KeyRegistry::build` ran once, in `App::new`. That was correct while the Runs pane held
        // whatever `seed_from_journal` produced at connect time — the registry and the view were
        // built from the same snapshot and could not disagree. **Making `/runs` live broke that
        // invariant**: the pane filled with runs the boot-time view had never heard of, every one
        // of them drew a hotkey on its border, and `keys.resolve` missed every single one.
        //
        // §B10 names this exactly: *"an implementation that starts focused in the message field
        // makes every hotkey printed on every border inert on arrival, and the borders are then
        // lying."* Same lie, reached by a different route — the border advertises `(b)` and `b`
        // does nothing.
        //
        // Rebuilt only when the item keys actually move, because this runs on every republish.
        if item_keys(&view) != item_keys(&self.view) {
            match KeyRegistry::build(&view) {
                Ok(k) => self.keys = k,
                // **The previous registry is kept and the collision is SAID.** A conflict here is
                // not a startup error any more — the session is already running and a panic would
                // take the user's draft with it — but silently keeping a stale registry is how
                // this defect got in. Degrade visibly, name the remedy (invariant 4).
                Err(conflict) => {
                    self.view = view;
                    self.set_status_detail(format!(
                        "two panes claimed {conflict} — hotkeys are showing the previous set until                          one of them goes"
                    ));
                    if let Some(tab) = self.pending_pane_summary.take() {
                        self.client_note(crate::commands::pane_summary(&self.view, tab), Tone::Normal);
                    }
                    return;
                }
            }
        }
        self.view = view;
        // **Said here, not at dispatch.** The pane's summary counts what the pane holds, so it is
        // composed once the producer's answer is in the view rather than from what was there when
        // the key was pressed.
        if let Some(tab) = self.pending_pane_summary.take() {
            self.client_note(crate::commands::pane_summary(&self.view, tab), Tone::Normal);
        }
    }

    /// The meter frame to draw. `MeterSource::None` holds the last reported one.
    pub fn meter_frame(&self) -> marlowe_view::Frame {
        self.view.meter.resolve(self.last_meter)
    }

    /// Whether a tool line is expanded: the user's toggle if there is one, else the producer's
    /// default (§B6 auto-expands failures).
    pub fn is_expanded(&self, call: &marlowe_view::ToolCall) -> bool {
        *self.expanded.get(&call.id).unwrap_or(&call.expanded)
    }

    /// Consume the selection, reporting whether there was one.
    ///
    /// **Taking rather than reading** — a selection that survived the keystroke that acted on it
    /// would make the next character replace the buffer a second time.
    fn take_selection(&mut self) -> bool {
        std::mem::take(&mut self.select_all)
    }

    /// Whether the composer is showing a selection, for the renderer.
    pub fn composer_selected(&self) -> bool {
        self.select_all
    }

    /// How far back from the end the composer is scrolled, for the renderer.
    pub fn composer_scroll(&self) -> u16 {
        self.composer_scroll
    }

    /// Move the composer's view. **`Up` goes back through what was typed or pasted.**
    ///
    /// The field grows to [`crate::render::INPUT_ROWS_MAX`] and then pins to its last line, so
    /// before this there was no key that could reach line 1 of a forty-line paste. The text was in
    /// the buffer and unreachable on screen — which is the same defect the growth was added to fix,
    /// one size up: **giving a composer a cap without giving it a scroll just moves the cliff.**
    fn scroll_composer(&mut self, back: i32) -> Action {
        self.composer_scroll = if back > 0 {
            self.composer_scroll.saturating_add(back as u16)
        } else {
            self.composer_scroll.saturating_sub((-back) as u16)
        };
        // No clamp here: `draw` clamps against the height it actually has, so an over-scroll
        // cannot render out of range and this needs nothing from the previous frame.
        Action::Redraw
    }

    /// A block arriving from the terminal's bracketed paste.
    ///
    /// Short pastes go in literally — collapsing two words would be a chip where the text is
    /// shorter than the chip. Anything big enough to bury the composer becomes one.
    pub fn paste(&mut self, text: String) {
        // A paste is an edit, and the composer is where edits go — so it takes the focus too,
        // rather than the text landing in a field the user is not looking at.
        self.focus = RegionId::Message;
        let lines = text.lines().count();
        if lines < crate::commands::PASTE_MIN_LINES
            && text.chars().count() < crate::commands::PASTE_MIN_CHARS
        {
            self.input.push_str(&text);
            return;
        }
        self.input
            .push_str(&crate::commands::paste_marker(self.pastes.len(), &text));
        self.pastes.push(text);
    }

    /// Put every minted chip back, so what is sent is what was pasted.
    ///
    /// **The transcript gets the full text, not the chip.** A conversation is a record of what was
    /// said; a composer affordance that survived into the record would make the transcript
    /// disagree with what the model received, and this project has spent enough sessions on two
    /// answers to one question.
    fn expand_pastes(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (i, body) in self.pastes.iter().enumerate() {
            out = out.replace(&crate::commands::paste_marker(i, body), body);
        }
        out
    }

    /// Whether the composer ends in a chip, and which. Backspace deletes the whole thing: half a
    /// chip is a marker that no longer matches, which would send the literal text `[Pasted #1 +4`
    /// and silently drop the paste.
    fn trailing_paste(&self) -> Option<(usize, String)> {
        self.pastes.iter().enumerate().rev().find_map(|(i, b)| {
            let m = crate::commands::paste_marker(i, b);
            self.input.ends_with(&m).then_some((i, m))
        })
    }

    /// Take the requests built up since the last drain. The driver applies them to a producer.
    pub fn drain_intents(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.outbox)
    }

    /// Ask the producer for something. **The only way this surface causes anything.**
    fn ask(&mut self, intent: Intent) {
        self.outbox.push(intent);
    }

    /// Show a producer's refusal. **Persistent, not a transient notice.**
    ///
    /// `notice` is cleared by the next keystroke, which is right for "copied 412 characters" and
    /// wrong for "that action was refused and here is why" — a user who looks away for a second
    /// would see a blocked action with no explanation, which is the silent no-op wearing a hat.
    /// So it lands in the conversation as a client line and stays there.
    pub fn refused(&mut self, e: &marlowe_view::IntentError) {
        self.client_note(e.as_notice(), Tone::Amber);
    }

    /// Replace §B5's detail line.
    ///
    /// **Exists so the driver has somewhere to say things that is not `stdout`.** Starting the
    /// daemon used to `eprintln!` after the alternate screen was up, writing raw text over the
    /// rendered frame — it survived until a resize forced a repaint, and read as a broken UI.
    /// Once the surface owns the terminal, the view is the only channel.
    pub fn set_status_detail(&mut self, detail: impl Into<String>) {
        self.view.status.detail = detail.into();
    }

    pub fn tab(&self) -> TabId {
        self.tab.into()
    }

    pub fn tree(&self) -> RegionTree {
        RegionTree::build(&self.view, self.tab)
    }

    /// True when the focused region takes typed characters as text.
    pub fn focus_is_text(&self) -> bool {
        match self.focus {
            RegionId::Message => true,
            RegionId::Item(tab, i) => crate::inspector::items_for(&self.view, self.tab)
                .get(i)
                .is_some_and(|item| item.editable && TabId::from(self.tab) == tab),
            _ => false,
        }
    }

    fn any_picker_open(&self) -> bool {
        self.picker_open.is_some()
    }

    fn close_pickers(&mut self) {
        self.picker_open = None;
    }

    /// The control a region id names, if it names one.
    ///
    /// **There is no `picker_mut`.** Selecting an option is [`Intent::Select`]; the surface reads
    /// the options in order to draw them and asks the producer to change which one is live.
    pub fn control_of(id: RegionId) -> Option<ControlId> {
        Some(match id {
            RegionId::Model => ControlId::Model,
            RegionId::Profile => ControlId::Profile,
            RegionId::Session => ControlId::Session,
            RegionId::Workspace => ControlId::Workspace,
            RegionId::Autonomy => ControlId::Autonomy,
            _ => return None,
        })
    }

    /// The five control-strip regions, in the order they are laid out. One list, so the mouse
    /// hit-test cannot drift out of step with the render order.
    pub const STRIP: [RegionId; 5] = [
        RegionId::Model,
        RegionId::Profile,
        RegionId::Session,
        RegionId::Workspace,
        RegionId::Autonomy,
    ];

    /// `(running, due today)` — the ambient counts. **One definition, two readers.**
    ///
    /// The titlebar row and the OS window title say the same thing, so they compute it in the same
    /// place. Two copies of "how many runs are running" is precisely the kind of duplication that
    /// disagrees six months later, with no test able to see it because each is individually right.
    pub fn run_counts(&self) -> (usize, usize) {
        let running = self
            .view
            .runs
            .iter()
            .filter(|i| i.lines.iter().any(|(l, _)| l.contains("running")))
            .count();
        let due = self
            .view
            .schedule
            .iter()
            .find(|i| i.label == "Due today")
            .map(|i| i.lines.len())
            .unwrap_or(0);
        (running, due)
    }

    /// The window title, as a function of session state. Written by the loop via OSC 0.
    ///
    /// **The title bar is a row of screen Marlowe is paying for either way.** Left alone it holds a
    /// static label — the profile's name, or worse, a shell's idea of the working directory. Driven
    /// from state it becomes the same readout the titlebar row carries inside the frame, which is
    /// what makes the window legible when it is not the focused one: a taskbar entry reading
    /// `marlowe — thursday · running` is the one piece of Marlowe visible from another application.
    ///
    /// This is the same argument for putting state in a native title bar on a platform that has
    /// one, so the string is built here rather than in the terminal-specific code.
    pub fn window_title(&self) -> String {
        let s = &self.view;
        let mut t = format!("marlowe — {}", s.control.session.value());
        // The state is worth the characters only when it is not the resting one; a title that
        // always says `idle` has spent a row to say nothing.
        if s.status.state != StatusState::Idle {
            t.push_str(&format!(" · {}", s.status.state.name()));
        }
        let (running, _due) = self.run_counts();
        if running > 0 {
            t.push_str(&format!(" · {running} runs"));
        }
        t
    }

    /// The slash-command suggestions for what is currently typed, or empty.
    ///
    /// One definition, read by both the renderer and the key dispatch — otherwise `Tab` completes
    /// from one list while the user is looking at another.
    pub fn completions(&self) -> Vec<&'static crate::commands::Command> {
        if self.focus != RegionId::Message {
            return Vec::new();
        }
        match self.input.strip_prefix('/') {
            Some(rest) if !rest.contains(' ') => crate::commands::complete(rest),
            _ => Vec::new(),
        }
    }

    /// Accept the highlighted suggestion. `Tab`, and the reason `Tab` is safe to take here: with a
    /// slash command open the region cycle is not what the hands are reaching for.
    pub fn accept_completion(&mut self) -> Action {
        let hits = self.completions();
        let Some(cmd) = hits.get(self.completion.min(hits.len().saturating_sub(1))) else {
            return Action::None;
        };
        self.input = format!("/{} ", cmd.name);
        self.completion = 0;
        Action::Redraw
    }

    /// `y` — copy the focused thing. §B10.
    ///
    /// What "focused" means depends on where focus is, and in M1 the conversation has no per-line
    /// cursor (§B6 cursoring is M2, with real scrollback), so from the conversation this copies the
    /// **last turn**. The notice says which, rather than leaving the user to infer it — a copy that
    /// silently took something other than what was wanted is worse than one that refused.
    pub fn copy_focused(&mut self) -> Action {
        let (text, what) = match self.focus {
            RegionId::Item(_, _) => {
                let items = crate::inspector::items_for(&self.view, self.tab);
                let RegionId::Item(_, i) = self.focus else {
                    unreachable!()
                };
                match items.get(i) {
                    Some(item) => (
                        item.lines
                            .iter()
                            .map(|(l, _)| l.clone())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        "this item",
                    ),
                    None => return Action::None,
                }
            }
            _ => match self
                .view
                .transcript
                .iter()
                .rev()
                .find_map(crate::clipboard::entry_text)
            {
                Some(t) => (t, "the last turn"),
                None => {
                    self.notice = Some("nothing to copy yet".into());
                    return Action::Redraw;
                }
            },
        };
        self.notice = Some(format!("copied {what} — {} characters", text.chars().count()));
        self.pending_copy = Some(text);
        Action::Redraw
    }

    /// `Y` — copy the whole transcript as markdown. §B10.
    pub fn copy_transcript(&mut self) -> Action {
        let text = crate::clipboard::transcript_markdown(&self.view);
        self.notice = Some(format!(
            "copied the transcript as markdown — {} characters",
            text.chars().count()
        ));
        self.pending_copy = Some(text);
        Action::Redraw
    }

    /// Whether a region takes a hover highlight at all.
    ///
    /// The conversation does not: hovering a transcript conveys nothing, and the pointer crossing
    /// it would repaint a border on every motion event. Everything else the mouse can act on does.
    pub fn hoverable(id: RegionId) -> bool {
        !matches!(id, RegionId::Conversation)
    }

    /// Record the pointer's region, ignoring anything not hoverable. Returns whether it changed —
    /// the loop only needs to redraw when it did, which is what keeps motion cheap.
    pub fn set_hover(&mut self, id: Option<RegionId>) -> bool {
        let next = id.filter(|i| Self::hoverable(*i));
        let changed = self.hover != next;
        self.hover = next;
        changed
    }

    /// `(region, index in the strip, number of options)` for the open dropdown, if one is open.
    pub fn open_picker(&self) -> Option<(RegionId, usize, usize)> {
        let open = self.picker_open?;
        let (i, id) = Self::STRIP
            .iter()
            .enumerate()
            .find(|(_, id)| Self::control_of(**id) == Some(open))?;
        Some((*id, i, self.view.picker(open).options.len()))
    }

    /// Open the focused region's dropdown, if it has one. What a click on a control cell does.
    pub fn open_focused_picker(&mut self) -> Action {
        let id = self.focus;
        match Self::control_of(id) {
            Some(c) => {
                self.picker_open = Some(c);
                Action::Redraw
            }
            None => Action::None,
        }
    }

    /// Choose an option by index and commit — what `Enter` does, addressed directly.
    ///
    /// Lives here rather than in the event loop so that clicking an option is the *same* state
    /// transition as arrowing to it and pressing `Enter`, and so it can be tested without a
    /// terminal. An out-of-range index is ignored rather than clamped: a click that lands on the
    /// popup's border is not a request to pick the nearest option.
    pub fn choose_option(&mut self, index: usize) -> Action {
        let id = self.focus;
        let Some(control) = Self::control_of(id) else {
            return Action::None;
        };
        if index >= self.view.picker(control).options.len() {
            return Action::None;
        }
        // **Asked, not assigned.** The dropdown closes because closing it is the surface's own
        // business; the selection does not move until the producer publishes a view saying it did.
        self.picker_open = None;
        self.ask(Intent::Select { control, option: index });
        Action::Redraw
    }

    /// The single key entry point.
    ///
    /// **It takes no clock, and that is a C2d result rather than a simplification.** Every branch
    /// used to end in a mutation of the producer, and a mutation needs a timestamp. They now end
    /// in an `Intent`, and the producer stamps its own time because the producer is the thing with
    /// a journal. A `now_ms` left here for symmetry would be an unused argument in the one file
    /// most likely to grow a surface-side timestamp.
    pub fn on_key(&mut self, key: Key) -> Action {
        // Counted before any dispatch, so `--diagnostic` distinguishes "the key never arrived"
        // from "the key arrived and did nothing". Those have completely different causes and look
        // identical from outside the process.
        self.keys_seen += 1;
        self.last_key = format!("{key:?}");
        // A confirmation lasts until the user does something else. Taken before dispatch so that a
        // copy can set a fresh one on the way through.
        self.notice = None;

        // 1. §B9's overlay is modal. It is the only element permitted to dim the frame, and while
        //    it is up it is the only thing that answers a key.
        if self.view.pending_approval.is_some() {
            return self.on_pending_approval_key(key);
        }
        if self.view.approval.is_some() {
            return self.on_approval_key(key);
        }

        // 1a. **Anything that is not `^c` disarms the quit.** A confirmation is a pair of
        //     consecutive presses; one that survived an intervening keystroke would turn a
        //     `Ctrl-C` from five minutes ago into half of a quit.
        if !matches!(key, Key::Ctrl('c')) {
            self.quit_armed = false;
        }


        // 1b. **Two chords belong to a focused text field before they belong to the footer**, and
        //     both were doing the wrong thing inside one.
        //
        //     `Ctrl-A` was unbound inside a text field, so select-all did nothing at all.
        //
        //     **`Ctrl-V` is deliberately NOT intercepted here, and that is a live conflict rather
        //     than a decision.** It is `^v Voice` in the footer, and §B10 requires every
        //     Ctrl-modified footer key to work *from inside a text field* — `b13_keyboard.rs`
        //     asserts `^v` reaches all seven §B5 states while typing. Making it paste-aware failed
        //     that test, which is the acceptance suite doing its job: the chord is spoken for.
        //
        //     Paste still works, because on the primary platform `Ctrl-V` never reaches this
        //     function: Windows Terminal binds it and delivers `Event::Paste`. Giving the chord to
        //     paste outright means rebinding Voice, which is a §B8 footer change and needs a
        //     `DECISIONS.md` entry rather than a quiet edit here.
        if self.focus_is_text() {
            if key == Key::Ctrl('a') {
                self.select_all = !self.input.is_empty();
                return Action::Redraw;
            }
        }

        // 2. Ctrl- and Alt-modified footer keys never suspend, in any focus. That is what makes
        //    "reachable by keyboard alone" true even from inside a text field, and ADR-056's
        //    `Alt` namespace inherits the same guarantee rather than being a lesser one.
        if let Key::Ctrl(c) = key {
            return self.on_global_ctrl(c);
        }
        if let Key::Alt(c) = key {
            return self.on_global_alt(c);
        }

        // 3. An open dropdown owns the arrows and Enter.
        if self.any_picker_open() {
            return self.on_picker_key(key);
        }

        match key {
            // A slash command being typed owns Tab. Completing is what the hands expect there,
            // and the region cycle is still one Esc away.
            Key::Tab if !self.completions().is_empty() => self.accept_completion(),
            Key::Tab => {
                self.focus = self.tree().next(self.focus);
                Action::Redraw
            }
            Key::BackTab => {
                self.focus = self.tree().prev(self.focus);
                Action::Redraw
            }
            // Esc lets go of a selection before it does anything else — the same "back out one
            // level" §B10 gives it everywhere, applied to the newest level.
            Key::Esc if self.select_all => {
                self.select_all = false;
                Action::Redraw
            }
            Key::Esc => self.on_escape(),
            _ if self.focus_is_text() => self.on_text_key(key),
            _ => self.on_region_key(key),
        }
    }

    fn on_escape(&mut self) -> Action {
        if self.any_picker_open() {
            self.close_pickers();
            return Action::Redraw;
        }
        if self.focus_is_text() {
            self.focus = RegionId::Conversation;
            return Action::Redraw;
        }
        // The outermost level. Partial output is kept (§B10).
        self.ask(Intent::Interrupt);
        Action::Redraw
    }

    fn on_global_ctrl(&mut self, c: char) -> Action {
        // Consecutive, or it is not a confirmation. Taken before the match so `'c'` sees the value
        // the PREVIOUS press left and every other chord clears it.
        let armed = std::mem::take(&mut self.quit_armed);
        self.quit_armed = armed && c == 'c';
        match c {
            // **`Ctrl-V` is paste (ADR-056), and reaching this function means the terminal did
            // NOT paste.** Windows Terminal binds the chord and delivers `Event::Paste`, so this
            // arm fires only where that did not happen — and there is no portable way to read a
            // system clipboard from a TUI, so the honest answer names the fallback rather than
            // doing nothing or inventing a capability.
            'v' => {
                self.notice = Some(
                    "this terminal did not send a paste — try Shift-Insert, or right-click".into(),
                );
                Action::Redraw
            }
            'n' => {
                // **`^n` no longer rebuilds a session, because a surface cannot make one.** It was
                // `self.session = Session::new()` -- the single clearest instance of the surface
                // holding what the daemon owns. A new session is a producer's act; what this
                // really did was discard a transcript and call it one. It now clears what the
                // surface owns and says plainly that the rest is not built.
                self.input.clear();
                self.scroll = None;
                self.client_lines.clear();
                self.client_note(
                    Notice::NotBuilt {
                        capability: Capability::NewSession,
                        arrives: Milestone::M2SessionD,
                    },
                    Tone::Dim,
                );
Action::Redraw
            }
            'r' => self.switch_tab(Tab::Runs),
            't' => self.switch_tab(Tab::Trust),
            'l' => {
                self.client_note(
                    Notice::NotBuilt {
                        capability: Capability::LineageWalk,
                        arrives: Milestone::M2SessionD,
                    },
                    Tone::Dim,
                );
Action::Redraw
            }
            'u' => {
                self.run_command("undo", &["1"]);
                Action::Redraw
            }
            'k' => {
                // §B10's palette indexes sessions, models, skills, recent files and memory search.
                // None exist. Saying so is the honest surface; a palette over a stub index would
                // look like a feature and measure nothing.
                self.client_note(
                    Notice::NotBuilt {
                        capability: Capability::CommandPalette,
                        arrives: Milestone::M2C3,
                    },
                    Tone::Dim,
                );
Action::Redraw
            }
            // **Copy first, quit second, and never quit on one press.**
            //
            // `y` and `Y` remain §B10's copy keys and are unchanged; this is the chord the hand
            // reaches for, wired to the same OSC 52 path so there is no second clipboard.
            'c' => {
                if self.take_selection() {
                    let text = std::mem::take(&mut self.input);
                    let text = self.expand_pastes(&text);
                    self.pastes.clear();
                    // **The expanded text, not the chips.** Copying `[Pasted #1 +340 lines]` into
                    // someone's clipboard would hand them a label instead of their data.
                    self.notice =
                        Some(format!("copied the message — {} characters", text.chars().count()));
                    self.input = text.clone();
                    self.pending_copy = Some(text);
                    return Action::Redraw;
                }
                if !self.input.is_empty() {
                    // A draft with no selection: copy it whole rather than quitting on top of it.
                    let text = self.expand_pastes(&self.input.clone());
                    self.notice =
                        Some(format!("copied the message — {} characters", text.chars().count()));
                    self.pending_copy = Some(text);
                    return Action::Redraw;
                }
                if self.quit_armed {
                    return Action::Quit;
                }
                self.quit_armed = true;
                self.notice = Some("press ^c again to quit — runs keep going without you".into());
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// ADR-056's `Alt` namespace. One binding, and the ADR is explicit that it stays that way
    /// until another footer key has to move.
    fn on_global_alt(&mut self, c: char) -> Action {
        // An Alt chord is still a footer key, so it disarms a pending quit like every other.
        self.quit_armed = false;
        match c {
            // §B5's seven states, on demand. Required by M1's scope: every state must be reachable
            // without waiting for a script to arrive at it. It was `^v` until ADR-056.
            'v' => {
                self.ask(Intent::ForceState(self.next_state()));
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    fn switch_tab(&mut self, tab: Tab) -> Action {
        self.tab = tab;
        self.inspector_scroll = 0;
        // Focus follows the tab only if it was already inside the inspector; otherwise a `^r`
        // while typing would steal the cursor out of a half-written message.
        if matches!(self.focus, RegionId::Item(_, _)) {
            self.focus = RegionId::Item(tab.into(), 0);
        }
        Action::Redraw
    }

    fn on_region_key(&mut self, key: Key) -> Action {
        match key {
            // §B10's copy keys. Checked before the region registry, and `keys.rs` refuses to build
            // a registry that binds either of them — so this cannot be shadowed by a pane whose
            // items happen to start with the letter y.
            Key::Char('y') => self.copy_focused(),
            Key::Char('Y') => self.copy_transcript(),
            Key::Char(c) => match self.keys.resolve(c, self.tab()) {
                Some(Binding::Focus(id)) => {
                    self.focus = id;
                    Action::Redraw
                }
                Some(Binding::Tab(t)) => self.switch_tab(match t {
                    TabId::Runs => Tab::Runs,
                    TabId::Schedule => Tab::Schedule,
                    TabId::Sessions => Tab::Sessions,
                    TabId::Skills => Tab::Skills,
                    TabId::Trust => Tab::Trust,
                    TabId::Status => Tab::Status,
                }),
                None => Action::None,
            },
            Key::Enter => self.act(),
            Key::Up => self.move_within(-1),
            Key::Down => self.move_within(1),
            Key::Left | Key::Right => Action::None,
            _ => Action::None,
        }
    }

    /// `Enter` acts on the focused region.
    fn act(&mut self) -> Action {
        match self.focus {
            RegionId::Model
            | RegionId::Profile
            | RegionId::Session
            | RegionId::Workspace
            | RegionId::Autonomy => {
                let id = self.focus;
                self.picker_open = Self::control_of(id);
                Action::Redraw
            }
            RegionId::Status => {
                self.ask(Intent::ForceState(self.next_state()));
                Action::Redraw
            }
            RegionId::Conversation => {
                // **The thinking block expands on `Enter`, like a tool line.**
                //
                // It has no letter of its own, and that is not a compromise: every lowercase key
                // is reachable by the daemon's run-key pool, so a global letter would collide the
                // moment enough runs existed — `t` hit `test-suite`, `x` hit `pdf-export`, `k` hit
                // the Sessions pane, and the registry refused all three. §B6 already says "cursor
                // to a line, Enter for full output in place"; a thinking block is a line with more
                // behind it, which is the same affordance.
                // **Any reasoning block, not just the last one.** The first version only fired
                // when `Reasoning` was the final entry — which it almost never is, because the
                // answer follows it. The line advertised `↵` and nothing happened, which is worse
                // than not offering it.
                if self.has_reasoning() {
                    self.reasoning_expanded = !self.reasoning_expanded;
                    return Action::Redraw;
                }
                // §B6: cursor to a line, Enter for full output in place. M1 expands the last tool
                // group; per-line cursoring inside the transcript is M2 with real scrollback.
                if let Some(marlowe_view::Entry::Tools(calls)) = self
                    .view
                    .transcript
                    .iter()
                    .rev()
                    .find(|e| matches!(e, marlowe_view::Entry::Tools(_)))
                {
                    // The toggle lands in the surface's override map, not on the producer's
                    // ToolCall. Whether you opened a tool line is a property of looking at it;
                    // the producer's `expanded` stays the default it set (B6 auto-expands
                    // failures) and is what an untouched line falls back to.
                    let ids: Vec<(u64, bool)> =
                        calls.iter().map(|c| (c.id, !self.is_expanded(c))).collect();
                    for (id, want) in ids {
                        self.expanded.insert(id, want);
                    }
                }
                Action::Redraw
            }
            // **A run listed with no way to act on it is a listing, not a pane.** `/runs` showed
            // every run and offered nothing to do with one: the only route to a window was typing
            // `/watch <name>` from memory, which is the affordance the pane exists to replace.
            //
            // The two effects are Session F's split, unchanged: the **spawn** goes to the driver,
            // because a surface holds no process API, and the pane refresh goes to the producer.
            // Same pair `run_command("watch")` emits, so there is one path and not two.
            RegionId::Item(tab, i) if tab == self.tab.into() => {
                let run = crate::inspector::items_for(&self.view, self.tab)
                    .get(i)
                    .and_then(|item| item.id.clone());
                match run {
                    Some(run) => {
                        self.window_asks.push(Outcome::Watch(run.clone()));
                        self.ask(Intent::Watch { run });
                    }
                    // An item that names nothing cannot be opened, and saying so is better than a
                    // key that looks live and is not.
                    None => self.notice = Some("nothing to open here".into()),
                }
                Action::Redraw
            }
            RegionId::Item(_, _) => Action::Redraw,
            RegionId::Message => Action::None,
            // **A run window's regions, listed rather than swept into a `_`.** `RegionTree::build`
            // never yields one, so the main pane's focus cannot land here — but a catch-all would
            // also swallow the next region somebody adds to the conversation, which is the
            // "defaults that make a mismatch unobservable" family. Naming them keeps that arm a
            // compile error.
            RegionId::RunIdentity
            | RegionId::RunCheckpoint
            | RegionId::RunOutput
            | RegionId::RunSteer
            | RegionId::RunSubagents
            | RegionId::RunBudget
            | RegionId::RunScopeMemory
            | RegionId::RunMeetings => Action::None,
        }
    }

    fn move_within(&mut self, delta: i32) -> Action {
        match self.focus {
            RegionId::Conversation => {
                // `None` is pinned to the bottom and stays pinned as the transcript grows — which
                // is what a live conversation wants, and is not the same as an offset that happens
                // to equal the maximum.
                let max = self.scroll_max.get();
                if max == 0 {
                    // The transcript fits. Nothing to scroll, and inventing an offset here is how
                    // the previous version ended up 65,534 lines above a 20-line conversation.
                    return Action::None;
                }
                self.scroll = match (self.scroll, delta < 0) {
                    (None, true) => Some(max.saturating_sub(1)),
                    (Some(o), true) => Some(o.saturating_sub(1)),
                    // Already at the bottom, and scrolling down re-pins rather than drifting.
                    (None, false) => None,
                    (Some(o), false) if o + 1 >= max => None,
                    (Some(o), false) => Some(o + 1),
                };
                Action::Redraw
            }
            RegionId::Item(tab, i) => {
                let n = crate::inspector::items_for(&self.view, self.tab).len();
                let next = (i as i32 + delta).clamp(0, n.saturating_sub(1) as i32) as usize;
                self.focus = RegionId::Item(tab, next);
                // Follow the selection rather than leaving it behind the fold. Only when it has
                // actually gone off an edge — scrolling on every keypress would move the whole
                // pane under a user who is just stepping down one row.
                let next = next as u16;
                if next < self.inspector_scroll {
                    self.inspector_scroll = next;
                } else if next > self.inspector_last_visible.get() {
                    self.inspector_scroll = self
                        .inspector_scroll
                        .saturating_add(next - self.inspector_last_visible.get())
                        .min(self.inspector_scroll_max.get());
                }
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// Arrow keys inside an open dropdown.
    ///
    /// **The highlight moves; the value does not.** M1 assigned `p.selected` on every arrow press,
    /// so arrowing past `act` in the Autonomy dropdown *granted* `act` in passing and `Esc` left it
    /// there. Under Addendum A §A8 that is the worst possible place for the bug to live -- the one
    /// control that changes what Marlowe may do without asking, changing on a keystroke that was
    /// never a choice.
    ///
    /// The highlight is now `picker_cursor`, which is the surface's, and `Enter` is what asks.
    fn on_picker_key(&mut self, key: Key) -> Action {
        let Some(control) = self.picker_open else {
            self.close_pickers();
            return Action::Redraw;
        };
        let n = self.view.picker(control).options.len();
        match key {
            Key::Up => {
                self.picker_cursor = self.picker_cursor.saturating_sub(1);
                Action::Redraw
            }
            Key::Down => {
                self.picker_cursor = (self.picker_cursor + 1).min(n - 1);
                Action::Redraw
            }
            Key::Enter => {
                let option = self.picker_cursor;
                self.picker_open = None;
                self.ask(Intent::Select { control, option });
                Action::Redraw
            }
            Key::Esc => {
                // Backs out changing nothing, which is now true rather than aspirational.
                self.picker_open = None;
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    fn on_text_key(&mut self, key: Key) -> Action {
        let steering = self.focus != RegionId::Message;
        match key {
            Key::Char(c) => {
                if steering {
                    self.steer.push(c);
                } else {
                    // Typing over a selection replaces it, which is the whole reason `Ctrl-A` is
                    // worth having: select all, type, and the old message is gone.
                    if self.take_selection() {
                        self.input.clear();
                        self.pastes.clear();
                    }
                    self.input.push(c);
                    // Typing always shows what is being typed.
                    self.composer_scroll = 0;
                    // The list just changed underneath the index.
                    self.completion = 0;
                }
                Action::Redraw
            }
            // The composer's own scroll. The steer *item* is one row and has nothing to scroll,
            // so this is the message field's alone.
            Key::Up if !steering => self.scroll_composer(1),
            Key::Down if !steering => self.scroll_composer(-1),
            Key::CtrlBackspace => {
                if steering {
                    delete_word_back(&mut self.steer);
                } else if self.take_selection() {
                    self.input.clear();
                    self.pastes.clear();
                } else if let Some((i, marker)) = self.trailing_paste() {
                    // A chip is one word to the eye, so a word-delete takes all of it.
                    self.input.truncate(self.input.len() - marker.len());
                    if i + 1 == self.pastes.len() {
                        self.pastes.pop();
                    }
                } else {
                    delete_word_back(&mut self.input);
                }
                self.composer_scroll = 0;
                self.completion = 0;
                Action::Redraw
            }
            Key::Backspace => {
                if steering {
                    self.steer.pop();
                } else if self.take_selection() {
                    self.input.clear();
                    self.pastes.clear();
                    self.completion = 0;
                } else {
                    // **A chip deletes whole.** Popping one character leaves a marker that no
                    // longer matches, so the paste it stood for would be silently dropped and the
                    // literal wreckage sent instead.
                    match self.trailing_paste() {
                        Some((i, marker)) => {
                            self.input.truncate(self.input.len() - marker.len());
                            // **Popped, never `remove`d.** A `remove` in the middle reindexes
                            // every later paste, so the chips still in the composer would expand
                            // to the wrong text. Only a trailing chip can be the last index; any
                            // other is left in place, unreferenced and harmless.
                            if i + 1 == self.pastes.len() {
                                self.pastes.pop();
                            }
                        }
                        None => {
                            self.input.pop();
                        }
                    }
                    self.completion = 0;
                }
                Action::Redraw
            }
            // Arrows walk the suggestions when there are any, and do nothing otherwise — a text
            // field with no completion open has no other use for Up and Down in M1.
            Key::Up | Key::Down if !self.completions().is_empty() => {
                let n = self.completions().len();
                self.completion = if key == Key::Up {
                    (self.completion + n - 1) % n
                } else {
                    (self.completion + 1) % n
                };
                Action::Redraw
            }
            // §B10: multiline by default — Shift-Enter for a newline, Enter to send.
            Key::ShiftEnter => {
                if steering {
                    self.steer.push('\n');
                } else {
                    self.input.push('\n');
                }
                Action::Redraw
            }
            Key::Enter => {
                if steering {
                    let text = std::mem::take(&mut self.steer);
                    if !text.trim().is_empty() {
                        // v1.0 §10.1: injected into a RUNNING child without killing it.
                        self.client_note(
                            Notice::NotBuilt {
                                capability: Capability::Steer,
                                arrives: Milestone::M3,
                            },
                            Tone::Normal,
                        );
                    }
                    return Action::Redraw;
                }
                self.submit()
            }
            _ => Action::None,
        }
    }

    /// Send the message field. Handles `/command`, `!shell`, and plain prose.
    ///
    /// **No clock.** Submitting used to stamp a turn onto the producer, which needed one. It now
    /// emits an `Intent` and the producer stamps its own time — which is the correct owner, since
    /// the producer is the thing with a journal.
    pub fn submit(&mut self) -> Action {
        let raw = std::mem::take(&mut self.input);
        let text = self.expand_pastes(raw.trim());
        self.pastes.clear();
        self.composer_scroll = 0;
        if text.is_empty() {
            return Action::None;
        }
        self.scroll = None;

        if let Some(rest) = text.strip_prefix('/') {
            let mut parts = rest.split_whitespace();
            let name = parts.next().unwrap_or("");
            let args: Vec<&str> = parts.collect();
            return match self.run_command(name, &args) {
                Outcome::Quit => Action::Quit,
                _ => Action::Redraw,
            };
        }

        if let Some(cmd) = text.strip_prefix('!') {
            // §B10: `!cmd` runs a shell command in the agent's working directory, **through the
            // same approval path**. M1 has no execution and no permission layer, so it reports
            // that rather than pretending — a stub that echoed fake output would be teaching the
            // user something false about what the approval path does.
            self.pending = Some(PendingLine {
                text: text.clone(),
                state: marlowe_view::PendingState::Rejected(
                    "shell runs through the approval path, which lands in M2".into(),
                ),
            });
            self.client_note(
                Notice::NotBuilt { capability: Capability::Shell, arrives: Milestone::M2C3 },
                Tone::Amber,
            );
            return Action::Redraw;
        }

        // §B10: input is never blocked, so the line is shown at once -- as PENDING, in its own
        // weight, retired only when the producer's transcript contains it. See
        // `marlowe_view::view`'s header for what happens if it never is.
        self.pending = Some(PendingLine::awaiting(text.clone()));
        self.ask(Intent::Send(text));
        Action::Redraw
    }

    /// Dispatch through the **one** command registry (§B11), and render the outcome the TUI way.
    /// `now_ms` is gone: the dispatcher is pure now, so a clock argument here would be one
    /// nobody reads and somebody eventually would.
    pub fn run_command(&mut self, name: &str, args: &[&str]) -> Outcome {
        let outcome = commands::dispatch(&self.view, name, args);
        match &outcome {
            Outcome::Say(notice) => self.client_note(notice.clone(), Tone::Normal),
            // Diagnostics are not speech and do not go through the Notice vocabulary. They render
            // dim, because a capability report needing nothing from the user should not pull the
            // eye (§B2).
            Outcome::Diagnostic(lines) => {
                self.diagnostic_lines = lines.clone();
            }
            // **Switch now, count later.** The tab is the user's keystroke and is instant; the
            // summary waits for the daemon, because the daemon is what knows.
            Outcome::TabLive(tab, intent) => {
                self.tab = *tab;
                self.inspector_scroll = 0;
                self.pending_pane_summary = Some(*tab);
                self.ask(intent.clone());
            }
            Outcome::Tab(tab, said) => {
                // §B7's rule, and the whole argument for a TUI over a chat log: the inspector
                // renders it and the conversation says only what a colleague would say out loud.
                self.tab = *tab;
                self.inspector_scroll = 0;
                self.client_note(said.clone(), Tone::Normal);
            }
            // A command that asks for something: show what was asked, queue the request. The
            // producer's next view is what says whether it happened.
            // A request carries no line of its own: the confirmation is the view coming back
            // changed. A surface narrating what it asked for was reporting a result it did not have.
            Outcome::Ask(intent) => {
                let intent = intent.clone();
                self.ask(intent);
            }
            Outcome::Rejected(refusal) => {
                self.client_note(Notice::Refused(refusal.clone()), Tone::Amber)
            }
            Outcome::Unknown(name) => {
                let nearest = commands::complete(name).first().map(|c| c.name);
                self.client_note(
                    Notice::Refused(Refusal::UnknownCommand {
                        name: Echo::new(name.clone()),
                        nearest,
                    }),
                    Tone::Amber,
                );
            }
            Outcome::Quit => {}
            // **The driver acts on these, not the App.** `marlowe-surface` has no process API and
            // no socket, and giving it one would be the surface holding the control plane. They are
            // returned to the caller, which is the TUI's event loop; a `_` arm here would swallow
            // the next `Outcome` somebody adds instead.
            // **Two effects, and both are Session A's design read literally.** A's
            // `Intent::Watch` refreshes the Runs pane for this run; A's own note says *"the window
            // is Session F's; the state it renders is this — one state, two renderings"*. So the
            // pane refresh goes to the producer exactly as A wired it, and the **spawn** goes to
            // the driver, because a producer cannot start a process (`marlowe-stub` implements
            // `Produce` too, and must not grow the ability).
            Outcome::Watch(run) => {
                self.window_asks.push(outcome.clone());
                self.ask(Intent::Watch { run: run.clone() });
            }
        }
        outcome
    }

    /// Take what the driver has to act on. See [`App::window_asks`].
    pub fn drain_window_asks(&mut self) -> Vec<crate::commands::Outcome> {
        std::mem::take(&mut self.window_asks)
    }

    /// A line the **driver** produced, once it knows what happened.
    ///
    /// Public because the driver is the half that can open a window and reach a socket, and the
    /// answer to `/watch` is not knowable until it has tried. It takes a [`Notice`] and not a
    /// `String`, so the closed vocabulary still holds: a driver cannot compose prose here any more
    /// than a command can.
    pub fn note(&mut self, notice: Notice, tone: Tone) {
        self.client_note(notice, tone);
    }

    /// Emit a line **the client produced**. Not Marlowe speaking, and not transcript.
    ///
    /// This replaces M1's `say()`, which pushed `Entry::Said` -- so `/help` output, `No /foo.`
    /// and `Opened for editing.` were all rendered as Marlowe's own prose in the confirmed
    /// conversation. That broke two rules at once: a surface authoring session state
    /// (ARCHITECTURE 2.14) and a surface authoring persona-bearing prose (CLAUDE.md's third fixed
    /// decision). A `/help` listing is the tool answering, and it is not part of the conversation.
    /// **The signature is the enforcement.** There is no `String` in it, so a surface cannot send
    /// prose — it can only name a [`Notice`] variant, and the variants are closed. That is
    /// stronger than a rule saying it shouldn't: `client_note("whatever I like")` does not compile.
    fn client_note(&mut self, notice: Notice, tone: Tone) {
        let after = self.view.transcript.len();
        self.client_lines.push(ClientLine::new(notice, tone, after));
    }

    /// Whether the transcript currently holds a thinking block.
    ///
    /// `t` is only claimed when there is one, so it stays available to the region registry
    /// otherwise — a global key that does nothing most of the time is a key the user stops
    /// trusting.
    pub fn has_reasoning(&self) -> bool {
        self.view
            .transcript
            .iter()
            .any(|e| matches!(e, marlowe_view::Entry::Reasoning { .. }))
    }

    /// The next state in B5's order -- what `^v` and Enter-on-the-status-band ask for.
    ///
    /// The surface computes the *request*; the producer decides whether to honour it. A real
    /// daemon refuses `ForceState` by name (`IntentError::NotADemo`).
    fn next_state(&self) -> StatusState {
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
        ORDER[(i + 1) % ORDER.len()]
    }

    /// The live window: **yes, no, or no-with-a-reason.**
    ///
    /// There is no dismiss key and that is deliberate. The daemon is blocked on this answer, so a
    /// window that could be closed without sending one would hang the turn with nothing on screen
    /// explaining why.
    fn on_pending_approval_key(&mut self, key: Key) -> Action {
        // Typing a reason. Enter sends the decline with it; Esc goes back to the three keys.
        if let Some(buf) = self.decline_reason.as_mut() {
            match key {
                Key::Char(c) => {
                    buf.push(c);
                    return Action::Redraw;
                }
                Key::Backspace => {
                    buf.pop();
                    return Action::Redraw;
                }
                Key::Enter => {
                    let reason = self.decline_reason.take().unwrap_or_default();
                    self.ask(Intent::Approve {
                        granted: false,
                        reason: Some(marlowe_view::notice::Echo(reason)),
                    });
                    return Action::Redraw;
                }
                Key::Esc => {
                    // Back to the question, NOT out of it.
                    self.decline_reason = None;
                    return Action::Redraw;
                }
                _ => return Action::None,
            }
        }

        match key {
            Key::Char('y') | Key::Char('Y') => {
                self.ask(Intent::Approve { granted: true, reason: None });
                Action::Redraw
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                self.ask(Intent::Approve { granted: false, reason: None });
                Action::Redraw
            }
            Key::Char('o') | Key::Char('O') => {
                self.decline_reason = Some(String::new());
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    fn on_approval_key(&mut self, key: Key) -> Action {
        match key {
            Key::Enter => {
                self.ask(Intent::Approve { granted: true, reason: None });
                Action::Redraw
            }
            Key::Esc => {
                self.ask(Intent::Approve { granted: false, reason: None });
                Action::Redraw
            }
            Key::Char('e') => {
                // **Declining and then noting what the user chose.** M1 cleared `approval` on the
                // producer directly, which dismissed the overlay whether or not anything had
                // actually been declined -- a surface deciding an approval outcome, which is the
                // one thing 8.2 says it must never do.
                self.ask(Intent::Approve { granted: false, reason: None });
                self.client_note(
                    Notice::ApprovalResolved {
                        disposition: marlowe_view::Disposition::OpenedForEditing,
                    },
                    Tone::Normal,
                );
                Action::Redraw
            }
            Key::Char('s') => {
                // Addendum A §A3: sending *as Marlowe* is the path that avoids impersonation
                // entirely, and §B9 requires the overlay to offer it.
                //
                // **Send-as-Marlowe is not yet a distinct intent, and this says so rather than
                // pretending.** It declines the impersonating send; the delegated one needs a
                // producer that can perform it, which is Session E.
                self.ask(Intent::Approve { granted: false, reason: None });
                self.client_note(
                    Notice::NotBuilt {
                        capability: Capability::SendAsMarlowe,
                        arrives: Milestone::M2SessionE,
                    },
                    Tone::Amber,
                );
                Action::Redraw
            }
            // Everything else is swallowed. §B9's whole subject is a user who has stopped reading;
            // an overlay that let an unrelated keystroke fall through to the frame behind it would
            // be the rubber-stamping failure with extra steps.
            _ => Action::None,
        }
    }
}

/// Every item hotkey a view would claim, in pane order.
///
/// The cheap half of "did the registry go stale": rebuilding on every republish would walk six
/// panes per crank, and the answer only changes when a run appears, finishes or is renamed.
fn item_keys(view: &SessionView) -> Vec<(u8, Option<char>)> {
    marlowe_view::Tab::ALL
        .iter()
        .flat_map(|tab| {
            crate::inspector::items_for(view, *tab)
                .into_iter()
                .map(move |i| (*tab as u8, i.key))
        })
        .collect()
}

/// Delete back to the start of the previous word.
///
/// **Trailing whitespace first, then the word**, which is what every editor does and what the hand
/// expects: `Ctrl-Backspace` after `hello world ` leaves `hello `, not `hello world`.
pub(crate) fn delete_word_back(s: &mut String) {
    while s.ends_with(char::is_whitespace) {
        s.pop();
    }
    while !s.is_empty() && !s.ends_with(char::is_whitespace) {
        s.pop();
    }
}
