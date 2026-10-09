//! `SessionView` — what a producer publishes, and the **only** thing a surface renders.
//!
//! # The direction of every field here is one-way
//!
//! ARCHITECTURE §2.14: *surfaces own rendering and input; they never hold policy, and never hold
//! state the daemon does not have.* M1 asserted that in two module headers and did not have it:
//! `App` owned a `marlowe_stub::Session` **mutably** and pushed into its transcript, so the surface
//! could and did author conversation turns no producer ever saw. In-process against a stub that is
//! invisible. Against a daemon on a socket it is the surface inventing history.
//!
//! So the split is now in the types. A surface holds a [`SessionView`] it cannot write to, and
//! changes anything by **asking** — [`Intent`] — which is a value it hands back to whatever is
//! driving it. Nothing in this crate can apply an `Intent`; only a producer can.
//!
//! # Optimistic state is allowed, and it is never allowed to become history
//!
//! A message field that showed nothing until a daemon round-trip would feel broken. So a surface
//! may render what the user just typed **before** it is acknowledged — as [`PendingLine`], which is
//! a different type, renders differently (§B2's dim weight plus an explicit marker), and has **no
//! transition into [`Entry`]**. There is deliberately no `PendingLine::confirm()`.
//!
//! The retirement rule is: a pending line goes away when the producer's `transcript` contains it,
//! and not before. If the producer never acknowledges, **it stays pending and stays visibly
//! pending, indefinitely** — the user can see that what they typed has not landed, which is the
//! truth. The failure mode this rules out is the quiet one: a surface that promotes its own
//! optimistic text to confirmed transcript after a timeout, producing a conversation that reads as
//! real and that no journal has a record of.

use crate::meter::MeterSource;
use crate::approval::BlastRadius;
use crate::model::{Ambient, ControlStrip, Entry, Item, Pager, StatusBand};

/// Everything a producer owns and a surface draws.
///
/// **No `Default`, and no constructor that invents values.** A surface able to conjure one of
/// these is a surface that can hold state the daemon lacks, and every test would stay green while
/// it did. Build one from [`crate::model`] parts in a producer, or do not build one.
///
/// Note what is *absent* and where it went: the selected inspector tab, whether a dropdown is
/// open, scroll offsets and expansion flags are all properties of **looking at** a session rather
/// than of the session, and they live on the surface's own `App`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub control: ControlStrip,
    pub status: StatusBand,
    /// Confirmed history. A surface never appends to this — see [`PendingLine`].
    pub transcript: Vec<Entry>,
    pub pager: Pager,
    pub ambient: Ambient,
    /// `Some` while §B9's overlay is up. The producer decides; the surface renders and reports the
    /// answer back as [`Intent::Approve`].
    pub approval: Option<BlastRadius>,
    /// `Some` while a **live** approval is waiting — see [`crate::approval::PendingApproval`] for
    /// why this is not the field above. The daemon is blocked on the answer, so this is the one
    /// piece of view state with a process waiting on the other end of it.
    pub pending_approval: Option<crate::approval::PendingApproval>,
    /// What the amplitude source reported this frame, **including that there is no source**.
    pub meter: MeterSource,
    pub runs: Vec<Item>,
    pub schedule: Vec<Item>,
    /// §B7's **Status** tab: *"model, provider, context, spend, connection health, degradation
    /// reasons, memory size, daemon uptime."*
    ///
    /// # This is where the daemon's announcements live, and it is not a new pane
    ///
    /// The daemon says real things on its way up — which engine is serving and how long it took to
    /// start, which provider stores and lists, that retrieval is write-only, that three
    /// interrupted runs can be resumed. Every one of those is on the list this tab is chartered
    /// with, and until now every one of them went to **stderr**, where a user who launched from
    /// the launcher (§B17) never sees it.
    ///
    /// So they land here, alongside the facts they qualify, rather than in a region of their own.
    /// §B2 is the constraint that settles it: a border must earn itself, and a second pane holding
    /// what this pane is already chartered to hold does not.
    ///
    /// **Diagnostics are still excluded.** The daemon prefixes its two kinds of line differently
    /// already — `marlowe:` for facts about the user's machine, `[dev]` for the outbound-request
    /// dump and the raw provider frames — and only the first kind crosses the wire. §B1's carve-out
    /// keeps instrumentation behind `--dev`; a 1.9-second engine start is not instrumentation, it
    /// is the answer to *why was that slow*.
    pub status_pane: Vec<Item>,
}

/// A line the user has typed and the producer has not yet confirmed.
///
/// Rendered distinctly from confirmed transcript — never merged into it, never silently promoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLine {
    pub text: String,
    pub state: PendingState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingState {
    /// Handed to the producer; not yet in the transcript. Renders dim with a `·` marker.
    AwaitingAck,
    /// The producer said it will not be accepted, and why. Renders in the failure tone.
    ///
    /// Distinct from `AwaitingAck` because "still going" and "will never arrive" call for
    /// different things from the user, and a single "pending" state says neither.
    Rejected(String),
}

impl PendingLine {
    pub fn awaiting(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            state: PendingState::AwaitingAck,
        }
    }

    /// Whether the producer's transcript has caught up with this line.
    ///
    /// Deliberately a **query over the view**, not a method that mutates anything: retirement is
    /// something the surface *observes*, never something it decides. The last matching `User`
    /// entry is what counts, so re-sending the same text after it was confirmed does not read as
    /// already-acknowledged.
    pub fn is_acknowledged_by(&self, view: &SessionView) -> bool {
        view.transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if *t == self.text))
    }

    /// The marker shown beside the text. §B6's vocabulary: specific, never the word "pending"
    /// alone.
    pub fn marker(&self) -> &str {
        match &self.state {
            PendingState::AwaitingAck => "· not yet acknowledged",
            PendingState::Rejected(_) => "· not delivered",
        }
    }
}

/// A line **the client produced** — command output, a rejection, a notice.
///
/// # This is not Marlowe speaking, and C2d is where that stopped being conflated
///
/// M1's `App::say()` pushed `Entry::Said` into the transcript for `/help` output, for
/// `No /foo. Closest is /bar.`, for `Opened for editing. Nothing sent.` — client-side text,
/// rendered as Marlowe's own prose, in the confirmed conversation. Two things were wrong with it
/// at once: the surface was authoring transcript (ARCHITECTURE §2.14), and it was authoring
/// **persona-bearing prose** (CLAUDE.md's third fixed decision), which is a stricter rule still.
///
/// A command's output is the *tool* answering, the way a shell prints to your terminal without
/// anyone claiming the shell said it. So it is a distinct type, rendered in a distinct weight, and
/// it never enters [`SessionView::transcript`] — a `/help` listing is not part of the conversation
/// and must not appear in a transcript copied with `Y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientLine {
    /// **A [`crate::notice::Notice`], never a string.** The first version of this type carried
    /// `text: String`, which meant the surface still composed prose — `say(String)` renamed. The
    /// signature is the enforcement: there is no way to pass a sentence in.
    pub notice: crate::notice::Notice,
    pub tone: crate::model::Tone,
    /// The transcript length when this was emitted, so it renders in the place it happened rather
    /// than always at the bottom. Without it, a command run three turns ago would drift down the
    /// pane as the conversation grew.
    pub after: usize,
}

impl ClientLine {
    pub fn new(notice: crate::notice::Notice, tone: crate::model::Tone, after: usize) -> Self {
        Self { notice, tone, after }
    }
}

/// What a surface asks a producer to do. **The whole outbound vocabulary.**
///
/// Every variant is a request. None of them is applied by the surface, and there is no method on
/// [`SessionView`] that takes one — a surface holding an `Intent` it cannot apply is the shape
/// that makes §2.14 structural instead of asserted.
///
/// The enum is closed and deliberately small. Anything a user can do that changes the session has
/// to appear here, which is what makes the answer to *"what can a surface cause?"* readable in one
/// place rather than distributed across a key dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Start a turn with this message.
    Send(String),
    /// Change a control-strip value. The producer decides whether it may.
    ///
    /// **This is a request even for `autonomy`, and especially for it.** Addendum A §A8 makes
    /// self-granted promotion structurally impossible, which requires that the thing rendering the
    /// control is not the thing that changes it.
    Select { control: ControlId, option: usize },
    /// §B9's answer. `granted` is the user's, never the model's.
    /// §B9's answer. `reason` is the *Other* key: a decline the user explained.
    ///
    /// It is `Option<Echo>` rather than `Option<String>` because it is text the user typed, and
    /// ADR-030's rule is that such text is carried quoted and never reworded.
    Approve { granted: bool, reason: Option<crate::notice::Echo> },
    /// §B10's `Esc` at the outermost level. Keeps partial output.
    Interrupt,
    /// §B10's `/undo N` — soft-delete the last N turns.
    Undo(usize),
    /// `/compact`. Announces itself inline and does not interrupt.
    Compact,
    /// `/state <name>` and `^v`: drive the status band to a state.
    ///
    /// **A demonstration affordance, and a real producer is expected to refuse it by name.** M1
    /// needed every §B5 state reachable without waiting for a script to arrive at one. A daemon
    /// driving a real run has no business being told what state it is in, and
    /// [`IntentError::NotADemo`] is what it answers — a named refusal rather than a silent no-op,
    /// because a control that appears to work and does nothing is worse than one that says no.
    ForceState(crate::model::StatusState),
    /// `/steer <run> <words>` — guidance for a run that is already going.
    ///
    /// **A steer is a WRITE**, and M3-DESIGN §6.1 is a correction of an earlier draft that called
    /// the run window read-only. So it is an `Intent` like every other write: the surface asks,
    /// the producer decides. A steer field in a window that reached the daemon directly would be
    /// a second write path skipping the one adjudication.
    ///
    /// The text is an [`crate::notice::Echo`] for `Approve`'s reason: it is what the user typed,
    /// carried quoted and never reworded (ADR-030).
    Steer { run: String, text: crate::notice::Echo },
    /// `/watch <run>` — open a window on a run.
    ///
    /// §6.6: *"`/watch` opens a window; it does not stream into the conversation pane."* Filling
    /// the main pane with agent output halts the conversation visually, which is what this
    /// milestone exists to stop.
    Watch { run: String },
    /// `/runs` — **ask the daemon what runs there are**, rather than re-reading what it said once.
    ///
    /// # This is here because the Runs tab had never shown a live run to anyone
    ///
    /// `/runs` was an `Outcome::Tab` and nothing else: it switched tab and re-summarised the
    /// client's cached view, which was last filled from `Event::Run` at daemon boot by
    /// `seed_from_journal`. So the pane showed a snapshot of daemon startup — the seeded
    /// *interrupted* runs — for the whole life of the session, and a run started afterwards never
    /// appeared. The daemon side was correct throughout; `turn()` registers every run.
    ///
    /// **The same family as `--status` reporting the client's own provider instead of the
    /// daemon's**, which M3 Session A fixed: a surface answering from local state instead of
    /// asking the thing that knows. `Intent::Watch` was already doing it correctly one match-arm
    /// over.
    Runs,
}

/// Why a producer refused an [`Intent`].
///
/// A refusal is always **named**. The alternative — a producer quietly dropping an intent it does
/// not implement — makes a working surface indistinguishable from a broken one, which is the
/// failure mode this project has recorded more than any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    /// The intent only means something against the scripted stub.
    NotADemo(&'static str),
    /// The argument did not name anything. Carries what the options were.
    NoSuchOption { control: ControlId, given: String },
    /// The option exists and the **producer** declined it. The reason travels on the status band's
    /// `degraded`, not here — see `Refusal::OptionUnavailable`.
    OptionUnavailable { control: ControlId, given: String },
    /// The producer cannot do this yet, and says which milestone owns it.
    ///
    /// **Typed, not a `String`** — for the same reason `Notice` is (ADR-030 §5), and so a refusal
    /// can be rendered through the one persona renderer rather than formatted at a call site.
    NotBuilt { capability: crate::notice::Capability, arrives: crate::notice::Milestone },
    /// The producer asked the daemon and it refused. **The reason goes on the band**, never here:
    /// ADR-030 §5 keeps `String`s out of the notice vocabulary, and the daemon's own words already
    /// have a channel. See `Refusal::TheDaemonDeclined`.
    DaemonRefused { intent: &'static str },
}

impl IntentError {
    /// The refusal as harness speech. **A refused intent is shown, never swallowed** — a blocked
    /// action with no explanation is the silent-no-op failure wearing a different hat.
    pub fn as_notice(&self) -> crate::notice::Notice {
        use crate::notice::{Capability, Milestone, Notice};
        match self {
            IntentError::NotBuilt { capability, arrives } => {
                Notice::NotBuilt { capability: *capability, arrives: *arrives }
            }
            IntentError::NotADemo(_) => Notice::NotBuilt {
                capability: Capability::ScriptedStateDriving,
                arrives: Milestone::M2SessionE,
            },
            IntentError::NoSuchOption { control, given } => {
                Notice::Refused(crate::notice::Refusal::NoSuchOption {
                    control: *control,
                    given: crate::notice::Echo::new(given.clone()),
                })
            }
            IntentError::OptionUnavailable { control, given } => {
                Notice::Refused(crate::notice::Refusal::OptionUnavailable {
                    control: *control,
                    given: crate::notice::Echo::new(given.clone()),
                })
            }
            IntentError::DaemonRefused { intent } => {
                Notice::Refused(crate::notice::Refusal::TheDaemonDeclined { command: intent })
            }
        }
    }
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntentError::NotADemo(what) => write!(
                f,
                "{what} drives the scripted stub and means nothing against a real run"
            ),
            IntentError::NoSuchOption { control, given } => {
                write!(f, "{} has no option {given:?}", control.name())
            }
            IntentError::OptionUnavailable { control, given } => write!(
                f,
                "{} cannot use {given:?} right now; the band says why",
                control.name()
            ),
            IntentError::DaemonRefused { intent } => {
                write!(f, "the daemon declined /{intent}; the band carries its reason")
            }
            IntentError::NotBuilt { capability, arrives } => write!(
                f,
                "{} is not built. It lands in {}.",
                capability.subject(),
                arrives.name()
            ),
        }
    }
}

/// Which control-strip field an [`Intent::Select`] is about. §B4's five, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlId {
    Model,
    Profile,
    Session,
    Workspace,
    Autonomy,
    /// `ollama` or `openrouter`. **Reachable by `/provider`, and not on the strip** — see
    /// [`crate::model::ControlStrip::provider`] for why, and note that it is therefore absent from
    /// [`ControlId::ALL`] on purpose rather than by omission.
    Provider,
}

impl ControlId {
    /// **The strip, and only the strip.** `Provider` is a `ControlId` that is not in here, so
    /// anything iterating `ALL` to lay out or draw controls keeps the five §B13 measures.
    /// Anything that needs *every* control — the command registry — names them itself.
    pub const ALL: [ControlId; 5] = [
        ControlId::Model,
        ControlId::Profile,
        ControlId::Session,
        ControlId::Workspace,
        ControlId::Autonomy,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ControlId::Model => "model",
            ControlId::Profile => "profile",
            ControlId::Session => "session",
            ControlId::Workspace => "workspace",
            ControlId::Autonomy => "autonomy",
            ControlId::Provider => "provider",
        }
    }
}

impl SessionView {
    /// The picker for a control. Read-only by construction — there is no `_mut` sibling, which is
    /// what makes [`Intent::Select`] the only route.
    pub fn picker(&self, id: ControlId) -> &crate::model::Picker {
        match id {
            ControlId::Model => &self.control.model,
            ControlId::Profile => &self.control.profile,
            ControlId::Session => &self.control.session,
            ControlId::Workspace => &self.control.workspace,
            ControlId::Autonomy => &self.control.autonomy,
            ControlId::Provider => &self.control.provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Picker;
    use crate::turn::DegradedPath;

    fn view(transcript: Vec<Entry>) -> SessionView {
        SessionView {
            // A default view has an empty pane: nothing has been announced yet.
            status_pane: Vec::new(),
            control: ControlStrip {
                model: Picker::new(&["a"], 0),
                profile: Picker::new(&["a"], 0),
                session: Picker::new(&["a"], 0),
                workspace: Picker::new(&["a"], 0),
                autonomy: Picker::new(&["observe"], 0),
                provider: Picker::new(&["ollama"], 0),
            },
            status: StatusBand {
                // No turn has run, so there is no cadence to show.
                cadence: None,
                state: crate::model::StatusState::Idle,
                detail: String::new(),
                figures: Vec::new(),
                degraded: None,
            },
            transcript,
            pager: Pager { turn: 0, compacted: 0, lineage: 0 },
            ambient: Ambient { fill_pct: 0, spend_cents: 0, elapsed_min: 0 },
            approval: None,
            pending_approval: None,
            meter: MeterSource::None,
            runs: Vec::new(),
            schedule: Vec::new(),
        }
    }

    #[test]
    fn a_pending_line_is_retired_by_the_producer_and_by_nothing_else() {
        let p = PendingLine::awaiting("read notes.md");
        assert!(!p.is_acknowledged_by(&view(Vec::new())));
        // Marlowe answering is NOT acknowledgement of the user's line.
        assert!(!p.is_acknowledged_by(&view(vec![Entry::Said(
            crate::notice::Speech::Model("ok".into()),
        )])));
        // Only the producer's own record of the user turn retires it.
        assert!(p.is_acknowledged_by(&view(vec![Entry::User("read notes.md".into())])));
    }

    #[test]
    fn there_is_no_way_to_turn_a_pending_line_into_transcript() {
        // The guard is structural rather than behavioural, so state it as one: `PendingLine` has
        // no method producing an `Entry`, and `SessionView.transcript` is only writable by
        // whoever owns the value. If a `confirm()` is ever added, this comment is the argument it
        // has to beat: a surface that promotes its own optimistic text produces a conversation
        // that reads as real and that no journal has a record of.
        let p = PendingLine::awaiting("x");
        assert_eq!(p.marker(), "· not yet acknowledged");
        let r = PendingLine {
            text: "x".into(),
            state: PendingState::Rejected("no daemon".into()),
        };
        // Two states, two markers: "still going" and "will never arrive" are different facts.
        assert_ne!(r.marker(), p.marker());
    }

    #[test]
    fn every_control_is_reachable_read_only_and_none_is_writable() {
        let v = view(Vec::new());
        for id in ControlId::ALL {
            assert_eq!(v.picker(id).options.len(), 1, "{}", id.name());
        }
        // Autonomy is a request like any other — §A8 forbids the renderer being the mutator.
        let i = Intent::Select { control: ControlId::Autonomy, option: 0 };
        assert!(matches!(i, Intent::Select { control: ControlId::Autonomy, .. }));
    }

    #[test]
    fn a_degraded_band_carries_the_specific_path_not_the_word_degraded() {
        let mut v = view(Vec::new());
        v.status.degraded = Some(DegradedPath::DenseRetrievalOffline);
        let headline = v.status.degraded.unwrap().headline();
        assert!(headline.contains("lexical only"), "{headline}");
        assert_ne!(headline, "degraded");
    }
}
