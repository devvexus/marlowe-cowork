//! **The closed vocabulary of harness-composed speech.**
//!
//! Marlowe says two kinds of thing. One comes from the model and arrives as tokens; the other is
//! the *harness* speaking — `No /foo`, `the palette is not built`, `undone: 2 turns`. This module
//! is the second kind, and it is an enum rather than a `String` for one reason:
//!
//! > A producer that cannot type-check what it renders is the same defect as a surface that
//! > composes prose — displaced by one layer, not fixed.
//!
//! M1 had `App::say(String)`. M2 C2d moved composition to the producer, which was necessary and
//! not sufficient: `Entry::Said(String)` still let anything write anything. Moving the **vocabulary**
//! is what closes it. See ADR-030.
//!
//! # Where a variant may be constructed, and why that is not the same question as who speaks
//!
//! The voice is always Marlowe's, rendered in exactly one place — [`Notice::render`]. What differs
//! is who holds the facts:
//!
//! * [`Notice::Listing`] and [`Notice::Refused`] are **surface-constructed**, because the surface
//!   already has everything they need. That is deliberate and it is a requirement, not an
//!   optimization: routing `/help` through a producer would make it a socket round-trip on the
//!   daemon path, and **a help command that waits is worse than one in the wrong voice.** No
//!   variant here ever reaches a model; nothing in this module can perform inference.
//! * The rest are **producer-constructed**, because their fields are facts only a producer has.
//!
//! # The rule that stops this becoming `say(String)` with extra steps
//!
//! **A variant is added only when a producer must say something the existing set cannot express —
//! never to carry a string the surface already has.** ADR-030 §5. The mechanical form of the rule
//! is that no field may be a `String`: a field is a compile-time `&'static str`, a typed value, or
//! an [`Echo`] of text the user themselves typed. `tests/notice_vocabulary.rs` enforces it.

use crate::view::ControlId;
use crate::model::Tab;

/// A command line the harness computed, quoted verbatim and never reworded.
///
/// # Why this is a third newtype and not an `Echo`
///
/// [`Echo`] means *text the user typed*, and this is not that — it is a path plus flags the harness
/// assembled. It is not prose either: it is a **datum the reader is meant to copy**, which is
/// exactly what `PathLabel` is for a resolved path, and it is a newtype for the same reason both of
/// those are — so ADR-030 §5's test can tell "we are quoting a fact" apart from "we composed a
/// sentence".
///
/// The one thing that must never happen to it is being reworded, and the type says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine(pub String);

impl CommandLine {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for CommandLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Text the **user** typed, echoed back verbatim.
///
/// The one legitimate runtime `String` in this module, and it is a newtype precisely so the
/// vocabulary test can tell "we are quoting the user" apart from "we composed a sentence". Nothing
/// here is ever reworded — quoting someone and speaking for them are different acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Echo(pub String);

impl Echo {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for Echo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Harness-composed speech. **Closed.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A table the client already holds. Surface-constructed, rendered instantly.
    Listing(Listing),
    /// The command exists or does not, and the argument did not work. Surface-constructed.
    Refused(Refusal),
    /// A capability that is correct, named, and unimplemented.
    ///
    /// **Not [`Refusal::UnknownCommand`]**, and the difference is the whole reason it is its own
    /// variant: that one means *no such thing*, this means *the right thing, not built yet*.
    /// Collapsing them would render `^k` as though the user had mistyped.
    NotBuilt { capability: Capability, arrives: Milestone },
    /// §B7: when the inspector changes, the conversation says what a colleague would say out loud.
    /// The only variant composed from live session data, which is why a producer owns it.
    PaneOpened { tab: Tab, summary: PaneSummary },
    /// §B9's outcome, after a decision the surface did not make.
    ApprovalResolved { disposition: Disposition },
    /// What a producer actually removed, which the surface cannot know.
    Undone { turns: u32 },
    /// A window opened on a run. `M3-DESIGN.md` §6.
    ///
    /// **The command is stated whether or not a window opened**, which is §6.7's whole shape: a
    /// terminal Marlowe could not open degrades to a copy-paste rather than to a broken button. So
    /// there is one variant with a `terminal` that may be absent, rather than a success variant and
    /// a failure variant that could drift into saying different things about the same event.
    WindowOpened { run: Echo, terminal: Option<Terminal>, command: CommandLine },
    /// A steer reached a run. `M3-DESIGN.md` §6.1 — a steer field is a **write**.
    ///
    /// It says *sent*, never *applied*: steering lands at the next iteration boundary, and a
    /// message claiming the run had already changed course would be a claim the surface cannot
    /// support.
    SteerSent { run: Echo },
    /// M3-DESIGN section 3.2: **a notification, and nothing else.**
    ///
    /// > *"He cannot read it, cannot query it, cannot summarise it. A window opens: the user and
    /// > the top-agent, directly. Marlowe is not in the room."*
    ///
    /// The containment is in the FIELDS. There is no body, no sentence, no option list and no
    /// category -- not because a producer was asked not to send them, but because there is nowhere
    /// to put them. `EscalationDesk::secretary_notice` returns this type and nothing else, and no
    /// method on the desk hands text to a loop, so *"cannot read it"* is a fact about a signature
    /// rather than about a registry entry a future `recall` variant or a debugging path could
    /// reach.
    ///
    /// `by` is composed by the producer as a harness word plus `marlowe_loop::run::sayable` --
    /// **zero model bytes reach it**. A model-chosen display name here could read `Marlowe`,
    /// `SYSTEM`, or the name of an adjacent run.
    ///
    /// Section 2.1 is why this matters more than it looks: Marlowe is a permanent run, ADR-023's
    /// floor is monotonic, and a Marlowe who ingests one finding can never compose a target again.
    /// The containment here is a ROUTING fact rather than a floor fact, which is what makes it
    /// hold whether or not the latch's scope is per-run or per-session.
    EscalationRaised { severity: marlowe_contract::EscalationSeverity, by: Echo },
}

/// Which terminal opened a window. A closed set, because the harness only knows how to drive the
/// ones it names — see `launcher::open_window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    WindowsTerminal,
}

impl Terminal {
    pub fn name(self) -> &'static str {
        match self {
            Terminal::WindowsTerminal => "Windows Terminal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Commands,
    Keys,
    /// A control-strip value and its options.
    Control(ControlId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `nearest` is a registry name, so it is `&'static str`; `name` is what the user typed.
    UnknownCommand { name: Echo, nearest: Option<&'static str> },
    /// Both fields come from the command registry's own table, never composed at a call site.
    Usage { command: &'static str, expects: &'static str },
    NoSuchOption { control: ControlId, given: Echo },
    /// A selectable option the **producer** declined — the model exists in the picker but the
    /// endpoint will not serve it right now.
    ///
    /// **Carries only the echoed name, no reason string.** ADR-030 §5: no `Notice` variant may hold
    /// a `String`, and the reason is not this type's to state — it is the daemon's, and it already
    /// travels on `StatusReport::degraded`, which the band renders. Two channels for one fact would
    /// be the second-source shape; this one says *which control and which value*, and the band says
    /// *why*.
    OptionUnavailable { control: ControlId, given: Echo },
    /// The producer asked the daemon and the daemon said no.
    ///
    /// **Carries the command and no reason string**, for `OptionUnavailable`'s reason and it is
    /// the same reason: ADR-030 §5 forbids a `String` in a `Notice`, and the detail is the
    /// daemon's rather than this type's. It travels on `StatusReport::degraded`, which the band
    /// renders. Two channels for one fact is the second-source shape; this one says *which
    /// command*, and the band says *why*.
    TheDaemonDeclined { command: &'static str },
}

/// A capability that exists in the design and not yet in the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    NewSession,
    CommandPalette,
    LineageWalk,
    Shell,
    Steer,
    SendAsMarlowe,
    /// **Named with its blocker, because Session E inherits the exact protocol gap.**
    ApprovalOverWire,
    InterruptingALiveTurn,
    ScriptedStateDriving,
    Undo,
    CompactOnDemand,
    SessionsPane,
    SkillsPane,
    TrustPane,
    StatusPane,
}

impl Capability {
    /// The subject of the sentence. A noun phrase, never a sentence of its own — §C1 wants the
    /// answer in the first sentence and that sentence is assembled in `render`.
    pub fn subject(self) -> &'static str {
        match self {
            Capability::NewSession => "Starting a new session",
            Capability::CommandPalette => "The command palette",
            Capability::LineageWalk => "Walking the compaction lineage",
            Capability::Shell => "Running a shell command",
            Capability::Steer => "Steering a running child",
            Capability::SendAsMarlowe => "Sending as Marlowe",
            Capability::ApprovalOverWire => {
                "Answering an approval over the wire — the daemon's Approval frame carries no                  novelty reason and no ceiling, and both are required before a blast radius can                  be shown"
            }
            Capability::InterruptingALiveTurn => "Interrupting a live turn — the wire has no cancel frame",
            Capability::ScriptedStateDriving => "Driving the status band by hand",
            Capability::Undo => "Undo",
            Capability::CompactOnDemand => "Compaction on demand",
            Capability::SessionsPane => "The sessions pane",
            Capability::SkillsPane => "The skills pane",
            Capability::TrustPane => "The trust ledger",
            Capability::StatusPane => "The status pane",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Milestone {
    M2C3,
    M2SessionD,
    M2SessionE,
    M3,
    M4,
    M6,
}

impl Milestone {
    pub fn name(self) -> &'static str {
        match self {
            Milestone::M2C3 => "M2 C3",
            Milestone::M2SessionD => "M2 D",
            Milestone::M2SessionE => "M2 E",
            Milestone::M3 => "M3",
            Milestone::M4 => "M4",
            Milestone::M6 => "M6",
        }
    }
}

/// What the conversation says when a pane opens.
///
/// **The growth bound is ADR-007's seven nouns.** One arm per noun is the natural cap; an eighth
/// means the split is wrong rather than that the enum needs widening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSummary {
    Runs { running: u32, spend_cents: u32, ceiling_cents: u32 },
    /// `next_at` is a typed time, not a formatted string — the first place free text would leak in.
    Schedule { needing_you: u32, next_at: (u8, u8), next_is_conflict: bool },
    /// §B7's Status tab, once it holds the daemon's own announcements.
    ///
    /// **Counts, not the lines themselves.** ADR-030 §5 forbids a `String` field here, and the rule
    /// earns its keep in exactly this variant: the announcements are daemon-authored sentences, and
    /// letting one into the notice vocabulary would make `Notice::render` a passthrough for
    /// arbitrary text. What Marlowe says out loud is *how many need you*; the sentences themselves
    /// are in the pane, which is the whole of §B7's rule — the transcript carries judgment, the
    /// region carries data.
    Status { announcements: u32, needing_you: u32, degraded: bool },
    NotBuilt { arrives: Milestone },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Sent,
    Declined,
    OpenedForEditing,
}

impl Notice {
    /// **The one place harness prose is written.** Addendum C applies to every line below.
    ///
    /// §C1: the first sentence carries the answer, including when the answer is no. §C4: no
    /// opening compliment, no eagerness, no restating the question, no emoji.
    /// `tests/notice_persona.rs` checks every variant against a probe set rather than trusting
    /// this comment.
    pub fn render(&self, ctx: &RenderContext<'_>) -> Vec<String> {
        match self {
            Notice::Listing(l) => l.render(ctx),
            Notice::Refused(r) => vec![r.render()],
            Notice::NotBuilt { capability, arrives } => vec![format!(
                "{} is not built. It lands in {}.",
                capability.subject(),
                arrives.name()
            )],
            Notice::PaneOpened { tab, summary } => vec![summary.render(*tab)],
            Notice::ApprovalResolved { disposition } => vec![match disposition {
                Disposition::Sent => {
                    "Sent. The thread is in your drafts folder if you want the copy.".to_string()
                }
                Disposition::Declined => "Not sent. It stays in drafts.".to_string(),
                Disposition::OpenedForEditing => "Opened for editing. Nothing sent.".to_string(),
            }],
            Notice::Undone { turns } => vec![format!(
                "Undone: {turns} turn{}.",
                if *turns == 1 { "" } else { "s" }
            )],
            // §C1: the first sentence carries the answer, including when the answer is "not here".
            // The command is on its own line both ways, because a line the reader has to copy is
            // easier to copy when nothing else is on it.
            Notice::WindowOpened { run, terminal, command } => match terminal {
                Some(t) => vec![
                    format!("Watching {run} in {}.", t.name()),
                    format!("  {command}"),
                ],
                None => vec![
                    format!("I can't open a window here. Run this to watch {run}:"),
                    format!("  {command}"),
                ],
            },
            Notice::SteerSent { run } => {
                vec![format!("Sent to {run}. It applies at the run's next step.")]
            }
            // Section 3.2's sentence, and it says everything Marlowe is allowed to know. The
            // severity is a harness word from a closed enum; `by` is a harness-derived run name.
            // Nothing here is composed from the escalation's content, because this type was never
            // given any.
            Notice::EscalationRaised { severity, by } => {
                vec![format!("A {} escalation has been raised by {by}.", severity.word())]
            }
        }
    }
}

/// What a listing needs in order to render itself, supplied by the caller.
///
/// Passed in rather than reached for, because `marlowe-view` depends on nothing and must not grow
/// a dependency on the command registry or the key registry to print a table.
pub struct RenderContext<'a> {
    /// `(left column, description)` for the command table.
    pub commands: &'a [(String, &'static str)],
    /// `(key, what it reaches)` for the key table.
    pub keys: &'a [(String, String)],
    /// The options of the control being listed, and which is live.
    pub control: Option<(&'a [String], usize)>,
}

impl Listing {
    fn render(&self, ctx: &RenderContext<'_>) -> Vec<String> {
        match self {
            Listing::Commands => {
                let width = ctx.commands.iter().map(|(l, _)| l.len()).max().unwrap_or(12);
                ctx.commands
                    .iter()
                    .map(|(left, description)| format!("{left:width$}  {description}"))
                    .collect()
            }
            Listing::Keys => ctx.keys.iter().map(|(k, what)| format!("{k}  {what}")).collect(),
            Listing::Control(id) => match ctx.control {
                Some((options, selected)) => vec![format!(
                    "{}: {}   ({})",
                    id.name(),
                    options.get(selected).map(String::as_str).unwrap_or("—"),
                    options.join(" · ")
                )],
                // A listing asked for without its data is a caller bug, and it says so rather
                // than printing an empty line that reads like "there are no options".
                None => vec![format!("{}: no options were supplied to render.", id.name())],
            },
        }
    }
}

impl Refusal {
    fn render(&self) -> String {
        match self {
            Refusal::UnknownCommand { name, nearest } => match nearest {
                Some(c) => format!("No /{name}. Closest is /{c}."),
                None => format!("No /{name}. /help lists what there is."),
            },
            Refusal::Usage { command, expects } => {
                format!("/{command} needs an argument: {expects}.")
            }
            Refusal::NoSuchOption { control, given } => {
                format!("{} has no option {given:?}.", control.name())
            }
            // **Points at the band rather than restating it.** The reason is the daemon's and
            // already renders there; saying it twice would be two sources for one fact, and the
            // one here would be a copy that can go stale.
            Refusal::OptionUnavailable { control, given } => format!(
                "{} cannot use {given:?} right now. The band says why.",
                control.name()
            ),
            Refusal::TheDaemonDeclined { command } => {
                format!("The daemon declined /{command}. The band says why.")
            }
        }
    }
}

impl PaneSummary {
    fn render(self, tab: Tab) -> String {
        match self {
            PaneSummary::Runs { running, spend_cents, ceiling_cents } => {
                if running == 0 {
                    "Nothing running.".to_string()
                } else {
                    format!(
                        "{running} running. The deep dive is at ${}.{:02} of its ${} ceiling.",
                        spend_cents / 100,
                        spend_cents % 100,
                        ceiling_cents / 100
                    )
                }
            }
            PaneSummary::Schedule { needing_you, next_at, next_is_conflict } => {
                let (h, m) = next_at;
                if needing_you == 0 {
                    "Nothing needs you today.".to_string()
                } else if next_is_conflict {
                    format!(
                        "{needing_you} things need you — the {h:02}:{m:02} is a conflict and is \
                         the one to look at."
                    )
                } else {
                    format!(
                        "{needing_you} things need you — the {h:02}:{m:02} is the one to look at."
                    )
                }
            }
            // §C1: the first sentence carries the answer, and the answer here is whether anything
            // is wrong. §C4: no flattery, no reassurance — "all clear" on a healthy daemon is a
            // fact, and the degraded case names the count rather than softening it.
            PaneSummary::Status { announcements, needing_you, degraded } => {
                if needing_you > 0 {
                    format!(
                        "{needing_you} of {announcements} want a look. They are at the top.",
                    )
                } else if degraded {
                    "Something is degraded. The reason is here.".to_string()
                } else if announcements == 0 {
                    "The daemon has said nothing yet.".to_string()
                } else {
                    format!("Nothing wrong. {announcements} routine so far.")
                }
            }
            PaneSummary::NotBuilt { arrives } => format!(
                "{} is not built. It lands in {}.",
                match tab {
                    Tab::Sessions => Capability::SessionsPane,
                    Tab::Skills => Capability::SkillsPane,
                    Tab::Trust => Capability::TrustPane,
                    _ => Capability::StatusPane,
                }
                .subject(),
                arrives.name()
            ),
        }
    }
}

/// Who composed a line of Marlowe's prose.
///
/// **The type is the enforcement.** `Entry::Said(String)` let a producer write anything; this makes
/// the harness half closed while leaving the model half a `String`, because model output is a
/// `String` and pretending otherwise would be a fiction. The surface renders both identically — a
/// user must not see a seam — but nothing can widen the harness vocabulary without adding a
/// [`Notice`] variant and defending it against ADR-030 §5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Speech {
    /// Tokens from the model, carrying the persona via its system prompt.
    Model(String),
    /// The harness speaking. Closed.
    Harness(Notice),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RenderContext<'static> {
        RenderContext { commands: &[], keys: &[], control: None }
    }

    #[test]
    fn not_built_is_not_the_same_speech_act_as_an_unknown_command() {
        // The reason `NotBuilt` is its own variant rather than a `Refusal`. If these ever render
        // alike, `^k` starts telling the user they mistyped something they did not type.
        let not_built = Notice::NotBuilt {
            capability: Capability::CommandPalette,
            arrives: Milestone::M2C3,
        }
        .render(&ctx())
        .join(" ");
        let unknown = Notice::Refused(Refusal::UnknownCommand {
            name: Echo::new("palette"),
            nearest: None,
        })
        .render(&ctx())
        .join(" ");
        assert!(not_built.contains("not built"), "{not_built}");
        assert!(!not_built.starts_with("No /"), "{not_built}");
        assert!(unknown.starts_with("No /"), "{unknown}");
    }

    #[test]
    fn an_echo_is_quoted_and_never_reworded() {
        let r = Notice::Refused(Refusal::NoSuchOption {
            control: ControlId::Autonomy,
            given: Echo::new("god-mode"),
        })
        .render(&ctx())
        .join(" ");
        assert!(r.contains("god-mode"), "{r}");
    }

    #[test]
    fn a_pane_with_nothing_in_it_says_so_rather_than_rendering_an_empty_sentence() {
        let s = PaneSummary::Runs { running: 0, spend_cents: 0, ceiling_cents: 300 }
            .render(Tab::Runs);
        assert_eq!(s, "Nothing running.");
        let s = PaneSummary::Schedule { needing_you: 0, next_at: (0, 0), next_is_conflict: false }
            .render(Tab::Schedule);
        assert_eq!(s, "Nothing needs you today.");
    }

    #[test]
    fn money_renders_from_cents_rather_than_from_a_preformatted_string() {
        // The field is `u32` cents precisely so no caller can hand in "$1.20" and drift.
        let s = PaneSummary::Runs { running: 2, spend_cents: 120, ceiling_cents: 300 }
            .render(Tab::Runs);
        assert!(s.contains("$1.20"), "{s}");
        assert!(s.contains("$3 ceiling"), "{s}");
    }

    #[test]
    fn a_listing_without_its_data_says_so_rather_than_printing_nothing() {
        // An empty listing reads as "there is nothing", which is a claim. This is the
        // permissive-default shape and it is refused in the smallest place it could appear.
        let s = Notice::Listing(Listing::Control(ControlId::Model)).render(&ctx());
        assert!(s[0].contains("no options were supplied"), "{s:?}");
    }
}
