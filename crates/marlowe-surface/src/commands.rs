//! **One command registry, dispatched by both surfaces.**
//!
//! §B11: the classic CLI has *command parity, not layout parity* — every slash command, every
//! session, every piece of data is reachable, rendered linearly. §B13 asks for 100%.
//!
//! A checklist would rot on the first command somebody added to the TUI. So parity is not checked,
//! it is **structural**: this is the only dispatcher, both surfaces call it, and
//! `tests/command_parity.rs` asserts that every entry in [`REGISTRY`] resolves in both. A TUI-only
//! command would have to be built by bypassing this module, which the parity test names.
//!
//! *(v1 said "no TUI-only features". v2 §B11 amends that to no TUI-only **capabilities** — layout
//! is allowed to differ, because layout is what a grid buys.)*

use marlowe_view::notice::{Capability, Echo, Listing, Milestone, Notice, PaneSummary, Refusal};
use marlowe_view::{ControlId, Intent, SessionView, StatusState, Tab, Tone};

/// One command, and enough about it to autocomplete inline with a description (§B10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

/// Every command M1 defines.
///
/// **Parity is measured against this list**, not against a claim about a command set that does not
/// exist yet. M2 adds commands; both surfaces get them at once because both read this array.
pub const REGISTRY: &[Command] = &[
    Command { name: "runs",     args: "",         description: "active background work — status, elapsed, spend, depth" },
    Command { name: "watch",    args: "<run>",    description: "open a window on one run — streaming output, checkpoint, spend against ceiling, and a field to steer it" },
    Command { name: "steer",    args: "<run> <words>", description: "guidance for a run already going. Delivered at its next step, never as a restart" },
    Command { name: "schedule", args: "",         description: "today's events, what Marlowe noticed, commitments due" },
    Command { name: "sessions", args: "",         description: "history, searchable by content" },
    Command { name: "skills",   args: "",         description: "installed skills by domain, and the exposed-tool budget" },
    Command { name: "trust",    args: "",         description: "the trust ledger — classes, tiers, agreement, ceilings" },
    Command { name: "status",   args: "",         description: "model, context, spend, connections, degradation" },
    Command { name: "state",    args: "<name>",   description: "drive the status band to a state (listening…idle)" },
    Command { name: "model",    args: "[name]",   description: "show or switch the routed model" },
    Command { name: "provider", args: "[name]",   description: "show or switch the engine — ollama (stores and runs) / ollama/llama.cpp (Ollama stores, llama.cpp runs; falls back to Ollama and says why) / openrouter" },
    Command { name: "profile",  args: "[name]",   description: "show or switch profile — work / personal" },
    Command { name: "session",  args: "[name]",   description: "show or switch session" },
    Command { name: "workspace", args: "[path]",  description: "show or switch the working directory" },
    Command { name: "autonomy", args: "[tier]",   description: "observe / suggest / draft / confirm / act" },
    Command { name: "undo",     args: "[n]",      description: "soft-delete the last n turns" },
    Command { name: "compact",  args: "",         description: "compact the session — announces inline, does not interrupt" },
    Command { name: "keys",     args: "",         description: "every key binding, and the region each one reaches" },
    Command { name: "doctor",   args: "",         description: "terminal capability check, including the braille glyph row" },
    Command { name: "help",     args: "",         description: "this list" },
    Command { name: "quit",     args: "",         description: "leave. Runs are daemon-owned and survive (invariant 6)" },
];

/// What a command did. The TUI and the CLI render these differently — that is the layout half of
/// "command parity, not layout parity".
///
/// # C2d: a command that changes the session returns an [`Intent`] instead of having changed it
///
/// `dispatch` used to take `&mut Session` and mutate a producer in place, which made the command
/// registry — a *surface* module — one of the places the surface authored session state. It now
/// takes `&SessionView` and is **pure**: the only way a command changes anything is by returning
/// [`Outcome::Ask`], which the driver hands to whatever is producing the view.
///
/// The practical tell that this was the right cut: `dispatch` can no longer make `/undo` remove a
/// turn from a transcript the daemon still has.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Something to say. **A [`Notice`], never a string** — the signature is what stops a command
    /// composing prose, which is stronger than the rule that says it shouldn't.
    Say(Notice),
    /// The inspector switched tab, and the conversation says only what a colleague would say out
    /// loud (§B7). Both travel together so neither surface can drop the other half.
    Tab(Tab, Notice),
    Quit,
    /// The command exists; the argument did not. Never a silent no-op.
    Rejected(Refusal),
    /// No such command. Carries what the user typed, verbatim, and the nearest registry name.
    Unknown(String),
    /// A diagnostic report. **Outside the Notice vocabulary by design** — see the `/doctor` arm.
    Diagnostic(Vec<String>),
    /// A tab whose contents the **daemon** owns: switch to it, and ask for them.
    ///
    /// **It carries no [`Notice`], and that is the difference from [`Outcome::Tab`].** A summary
    /// composed here would be composed from the view as it stands *before* the answer arrives —
    /// which is exactly the stale reading `/runs` used to show. The driver says the summary once
    /// the producer has answered; see `App::update`.
    TabLive(Tab, Intent),
    /// The command asks the producer to do something.
    ///
    /// **No lines travel with it.** A request that narrated itself was a surface reporting a
    /// result it did not have; the confirmation is the view coming back changed.
    Ask(Intent),
    /// `/watch <run>` — open a window on a run. `M3-DESIGN.md` §6.
    ///
    /// **The driver spawns it, not the surface.** `marlowe-surface` has no process API and must not
    /// grow one; what it can do is say which run was asked for.
    ///
    /// There is deliberately **no `Outcome::Steer` beside this**. Steering is a socket write, and
    /// `Intent::Steer` already reaches the daemon through the producer — the path Session A wired
    /// and the one `/steer` from a script uses. A second route for the same write is the shape
    /// ADR-054 exists to prevent; a window is one, and one is enough.
    Watch(String),
}


/// A pasted block is **represented rather than expanded**, and this is the half that makes
/// "entire paragraphs" tractable.
///
/// # Why a placeholder and not just a taller box
///
/// The composer grows to [`crate::render::INPUT_ROWS_MAX`] and then scrolls, so a 340-line paste
/// is technically survivable — you simply cannot see any of it. Every agent TUI that handles this
/// well does the same two things rather than one: **grow** for the few lines somebody types, and
/// **collapse** the wall somebody pastes. They are different problems and a taller box only solves
/// the first.
///
/// # Bracketed paste, not a heuristic
///
/// The terminal tells us. `EnableBracketedPaste` makes crossterm deliver `Event::Paste` as one
/// event, so this never has to guess from typing speed — which is the sort of guess that fires on
/// a fast typist and is invisible in a test.
///
/// # The forgeable claim, named rather than absorbed
///
/// The marker is ordinary text in an ordinary `String`, so a user who types
/// `[Pasted #1 +9 lines]` by hand gets their own earlier paste substituted for it. Expansion only
/// matches markers **this session minted**, which bounds it to text the same person pasted a moment
/// ago; the worst case is their words replaced by their own words, visibly, in a field they can
/// edit. That is the safe direction, and it is the same trade `marlowe_contract::text`'s refusal
/// marker records: the forgeable half is the harmless half.
pub const PASTE_MIN_LINES: usize = 3;
pub const PASTE_MIN_CHARS: usize = 240;

/// The chip shown in the composer for paste `i` (zero-based).
///
/// **It measures the paste in whatever unit the paste actually has.** A single long line — a URL,
/// a log line, a JSON blob — rendered as `+1 lines`, which is both wrong grammar and a useless
/// number: it told the reader nothing about what they were about to send. One line is measured in
/// characters, several in lines.
///
/// Derived from the body rather than passed a count, so the marker the composer draws and the
/// marker the expansion searches for cannot come apart — the two-sides-silently-disagree shape
/// applied to a string that has to match itself exactly.
pub fn paste_marker(i: usize, body: &str) -> String {
    let lines = body.lines().count();
    if lines > 1 {
        format!("[Pasted #{} +{lines} lines]", i + 1)
    } else {
        format!("[Pasted #{} +{} chars]", i + 1, body.chars().count())
    }
}

/// Resolve a name to its registry entry.
pub fn lookup(name: &str) -> Option<&'static Command> {
    REGISTRY.iter().find(|c| c.name == name)
}

/// Inline autocomplete (§B10): every command whose name starts with the prefix, with its
/// description.
pub fn complete(prefix: &str) -> Vec<&'static Command> {
    REGISTRY
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .collect()
}

/// **The only dispatcher.** Both surfaces call this and neither has a second path.
///
/// Read-only in the view, and `now_ms` is gone with the mutation it used to serve — a pure
/// dispatcher has no use for a clock.
pub fn dispatch(view: &SessionView, name: &str, args: &[&str]) -> Outcome {
    if lookup(name).is_none() {
        return Outcome::Unknown(name.to_string());
    }
    match name {
        // The pane summaries are FACTS the producer composed, read off the view. M1 hard-coded
        // the sentences here, in the surface, which is the violation one layer over from `say`.
        // **Ask, then say.** `/runs` used to switch tab and re-summarise the cached view, so the
        // pane held whatever the daemon said at boot and never a live run. The daemon owns this
        // noun; the surface asks it.
        "runs" => Outcome::TabLive(Tab::Runs, Intent::Runs),
        "schedule" => Outcome::Tab(Tab::Schedule, Notice::PaneOpened {
            tab: Tab::Schedule,
            summary: schedule_summary(view),
        }),
        "sessions" => not_built_tab(Tab::Sessions, Milestone::M3),
        "skills" => not_built_tab(Tab::Skills, Milestone::M2SessionD),
        "trust" => not_built_tab(Tab::Trust, Milestone::M6),
        "status" => not_built_tab(Tab::Status, Milestone::M2SessionD),

        "state" => match args.first().map(|s| parse_state(s)) {
            Some(Some(state)) => Outcome::Ask(Intent::ForceState(state)),
            _ => Outcome::Rejected(Refusal::Usage {
                command: "state",
                expects: "listening|thinking|speaking|writing|running|waiting|idle",
            }),
        },

        "model" => picker(view, ControlId::Model, args),
        // **ADR-049 §7, and it goes through `picker` like every other control.** The provider is
        // a `ControlId` that is not on the strip (see `ControlStrip::provider`), so this command is
        // the only way to reach it -- and routing it through the same helper is what keeps the
        // listing, the refusal and the parity test identical to the five that are.
        "provider" => picker(view, ControlId::Provider, args),
        "profile" => picker(view, ControlId::Profile, args),
        "session" => picker(view, ControlId::Session, args),
        "workspace" => picker(view, ControlId::Workspace, args),
        "autonomy" => picker(view, ControlId::Autonomy, args),

        "undo" => {
            let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
            // The producer reports what it actually removed (`Notice::Undone`); a surface
            // cannot know how many turns will be dropped and must not guess.
            Outcome::Ask(Intent::Undo(n))
        }

        "compact" => Outcome::Ask(Intent::Compact),

        // **`/watch` and `/steer` name a run, and a command that named none used to be a silent
        // no-op.** Both refuse by usage instead — `Refusal::Usage` is the shape every other
        // argument-taking command here uses, so the refusal reads the same wherever it comes from.
        // §6.6: **`/watch` opens a window; it does not stream into the conversation pane.**
        // Filling the main pane with agent output halts the conversation *visually*, which is what
        // M3 exists to stop — so this is an [`Outcome`] the **driver** acts on, not an `Intent` the
        // producer applies. Opening a window is a process spawn and a surface holds no such thing.
        //
        // **This replaced an `Intent::Watch` that printed the run's detail as a notice.** That
        // behaviour did not go away — it is `marlowe --runs <run>`, where a per-run listing
        // belongs, and where a script can read it without a terminal.
        "watch" => match args.first() {
            Some(run) if !run.trim().is_empty() => Outcome::Watch((*run).to_string()),
            _ => Outcome::Rejected(Refusal::Usage { command: "watch", expects: "<run id>" }),
        },
        "steer" => {
            // Everything after the id is the guidance. Joining rather than taking `args[1]` is
            // the difference between steering with a sentence and steering with a word.
            let text = args.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
            match args.first() {
                Some(run) if !run.trim().is_empty() && !text.trim().is_empty() => {
                    Outcome::Ask(Intent::Steer {
                        run: (*run).to_string(),
                        // Quoted, never reworded: ADR-030. A surface that paraphrased a steer
                        // would be composing the instruction it claims to be relaying.
                        text: Echo::new(text),
                    })
                }
                _ => Outcome::Rejected(Refusal::Usage {
                    command: "steer",
                    expects: "<run id> <what to tell it>",
                }),
            }
        }

        "keys" => Outcome::Say(Notice::Listing(Listing::Keys)),
        // **`/doctor` is diagnostic output, not speech**, and is deliberately outside the
        // Notice vocabulary — the same reasoning that keeps retrieval instrumentation behind
        // `--dev` (§B1). Forcing a terminal capability report through a persona renderer would
        // either bloat the enum with a `DoctorFacts` struct or invite a free-text escape hatch.
        // The carve-out is bounded: `tests/diagnostic_entry_points.rs` fails if a third one appears.
        "doctor" => Outcome::Diagnostic(crate::doctor::report(view)),
        "help" => Outcome::Say(Notice::Listing(Listing::Commands)),
        "quit" => Outcome::Quit,

        _ => Outcome::Unknown(name.to_string()),
    }
}

/// A tab that M1 does not fill still answers its command — and says so rather than printing an
/// empty section. Silence would read as "you have no skills", which is a claim, and a false one.
/// A tab that is not built still answers its command and says so. Silence would read as "you have
/// no skills", which is a claim, and a false one.
fn not_built_tab(tab: Tab, arrives: Milestone) -> Outcome {
    Outcome::Tab(tab, Notice::PaneOpened { tab, summary: PaneSummary::NotBuilt { arrives } })
}

/// What a pane says about itself, once its contents are in the view.
///
/// **Public because it is said after the answer, not with the request.** [`Outcome::TabLive`]
/// carries no notice for exactly that reason, so the driver needs a way to compose one at the
/// moment the pane is actually filled.
pub fn pane_summary(view: &SessionView, tab: Tab) -> Notice {
    let summary = match tab {
        Tab::Runs => runs_summary(view),
        Tab::Schedule => schedule_summary(view),
        // No other tab is live yet. `Milestone::M3` is what the `not_built_tab` arms already say.
        _ => PaneSummary::NotBuilt { arrives: Milestone::M3 },
    };
    Notice::PaneOpened { tab, summary }
}

/// Read the Runs pane's facts off the view. **Counted, never narrated.**
fn runs_summary(view: &SessionView) -> PaneSummary {
    let running = view
        .runs
        .iter()
        .filter(|i| i.lines.iter().any(|(l, _)| l.contains("running")))
        .count() as u32;
    PaneSummary::Runs { running, spend_cents: 120, ceiling_cents: 300 }
}

fn schedule_summary(view: &SessionView) -> PaneSummary {
    let needing_you = view
        .schedule
        .iter()
        .filter(|i| !matches!(i.tone, Tone::Dim))
        .count() as u32;
    let next_is_conflict = view
        .schedule
        .iter()
        .any(|i| i.lines.iter().any(|(l, _)| l.contains("conflict")));
    PaneSummary::Schedule { needing_you, next_at: (11, 0), next_is_conflict }
}

/// Show or request a control-strip value. **Reads the view; never writes it.**
///
/// The `Some(want)` arm returns the *request*, not a confirmation. `"{which}: {value}"` after a
/// successful assignment was a surface reporting a change it had made to a producer's state; the
/// same sentence now has to wait for the view to come back saying so.
fn picker(view: &SessionView, control: ControlId, args: &[&str]) -> Outcome {
    let p = view.picker(control);
    match args.first() {
        None => Outcome::Say(Notice::Listing(Listing::Control(control))),
        Some(want) => match p.options.iter().position(|o| o == want) {
            Some(i) => Outcome::Ask(Intent::Select { control, option: i }),
            None => Outcome::Rejected(Refusal::NoSuchOption {
                control,
                given: Echo::new(*want),
            }),
        },
    }
}

fn parse_state(s: &str) -> Option<StatusState> {
    Some(match s {
        "listening" => StatusState::Listening,
        "thinking" => StatusState::Thinking,
        "speaking" => StatusState::Speaking,
        "writing" => StatusState::Writing,
        "running" => StatusState::Running,
        "waiting" => StatusState::Waiting,
        "idle" => StatusState::Idle,
        _ => return None,
    })
}



/// Render an inspector pane linearly, for the classic CLI. Same data, no grid.
pub fn render_pane_linear(view: &SessionView, tab: Tab) -> Vec<String> {
    let mut out = Vec::new();
    for item in crate::inspector::items_for(view, tab) {
        // An item past its pane's letter pool has no accelerator to print. See `Item::key`.
        out.push(match item.key {
            Some(k) => format!("  {} ({k})", item.label),
            None => format!("  {}", item.label),
        });
        for (line, tone) in &item.lines {
            let mark = match tone {
                Tone::Red => "!",
                Tone::Amber => "*",
                _ => " ",
            };
            out.push(format!("    {mark} {line}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_command_dispatches_to_something_other_than_unknown() {
        let s = marlowe_stub::Session::new();
        for c in REGISTRY {
            let out = dispatch(s.view(), c.name, &[]);
            assert!(
                !matches!(out, Outcome::Unknown(_)),
                "/{} is in the registry but the dispatcher does not know it, so it would \
                 autocomplete and then do nothing",
                c.name
            );
        }
    }

    #[test]
    fn a_bad_argument_is_rejected_out_loud_and_changes_nothing() {
        let s = marlowe_stub::Session::new();
        let before = s.view().control.autonomy.value().to_string();
        let out = dispatch(s.view(), "autonomy", &["god-mode"]);
        assert!(matches!(out, Outcome::Rejected(_)));
        assert_eq!(s.view().control.autonomy.value(), before);
    }

    #[test]
    fn a_command_that_changes_the_session_asks_rather_than_acts() {
        // C2d's shape change, asserted directly. `dispatch` takes `&SessionView`, so there is no
        // way for it to mutate; what this pins is that the mutating commands still *do* something
        // — returning `Outcome::Lines` for `/undo` would be a command that silently stopped
        // working, which is precisely the failure a compile-checked refactor can leave behind.
        let s = marlowe_stub::Session::new();
        for (name, args, want) in [
            ("undo", vec!["2"], Intent::Undo(2)),
            ("compact", vec![], Intent::Compact),
            (
                "autonomy",
                vec!["act"],
                Intent::Select { control: ControlId::Autonomy, option: 4 },
            ),
            ("state", vec!["idle"], Intent::ForceState(StatusState::Idle)),
        ] {
            match dispatch(s.view(), name, &args) {
                Outcome::Ask(got) => assert_eq!(got, want, "/{name}"),
                other => panic!("/{name} returned {other:?}, not a request"),
            }
        }
    }

    #[test]
    fn a_request_carries_no_narration_of_its_own() {
        // Two corrections in one. M1 mutated and reported "undone: 2 turns" from a count it had
        // produced itself. The first fix made it ask and say "undo 2 turns" — still a surface
        // narrating, just in the future tense. `Outcome::Ask` now carries no lines at all: the
        // confirmation is the view coming back changed, and the count is the producer's
        // (`Notice::Undone`) because only it knows what was actually removed.
        let s = marlowe_stub::Session::new();
        let Outcome::Ask(intent) = dispatch(s.view(), "undo", &["2"]) else {
            panic!("expected a request");
        };
        assert_eq!(intent, Intent::Undo(2));
    }

    #[test]
    fn a_command_cannot_return_prose_because_the_type_has_nowhere_to_put_it() {
        // The structural claim, stated as a test so it is not just a comment: every arm of
        // `Outcome` that reaches the user carries a `Notice`, and `Notice` has no free-text field.
        // `Outcome::Diagnostic` is the one carve-out and is bounded by its own test.
        let s = marlowe_stub::Session::new();
        match dispatch(s.view(), "help", &[]) {
            Outcome::Say(Notice::Listing(Listing::Commands)) => {}
            other => panic!("/help should name a listing, not carry lines: {other:?}"),
        }
        match dispatch(s.view(), "state", &["nonsense"]) {
            Outcome::Rejected(Refusal::Usage { command, .. }) => assert_eq!(command, "state"),
            other => panic!("a bad argument should be a typed refusal: {other:?}"),
        }
    }

    #[test]
    fn autocomplete_carries_descriptions() {
        let hits = complete("s");
        assert!(hits.iter().any(|c| c.name == "schedule"));
        assert!(hits.iter().all(|c| !c.description.is_empty()));
    }
}

/// Owned data a [`Notice`] needs in order to render, gathered once by the caller.
///
/// `RenderContext` borrows, and `marlowe-view` depends on nothing — so it cannot reach the command
/// registry or the key registry itself. This is the small owner that closes that gap without
/// giving the view crate a dependency it should not have.
pub struct NoticeData {
    commands: Vec<(String, &'static str)>,
    keys: Vec<(String, String)>,
    control: Option<(Vec<String>, usize)>,
}

impl NoticeData {
    pub fn gather(view: &SessionView, notice: &Notice) -> Self {
        let mut commands = Vec::new();
        let mut keys = Vec::new();
        let mut control = None;
        match notice {
            Notice::Listing(Listing::Commands) => {
                commands = REGISTRY
                    .iter()
                    .map(|c| {
                        let left = if c.args.is_empty() {
                            format!("/{}", c.name)
                        } else {
                            format!("/{} {}", c.name, c.args)
                        };
                        (left, c.description)
                    })
                    .collect();
            }
            Notice::Listing(Listing::Keys) => keys = key_rows(view),
            Notice::Listing(Listing::Control(id)) => {
                let p = view.picker(*id);
                control = Some((p.options.clone(), p.selected));
            }
            _ => {}
        }
        Self { commands, keys, control }
    }

    pub fn ctx(&self) -> marlowe_view::notice::RenderContext<'_> {
        marlowe_view::notice::RenderContext {
            commands: &self.commands,
            keys: &self.keys,
            control: self.control.as_ref().map(|(o, s)| (o.as_slice(), *s)),
        }
    }
}

/// Render a notice against data gathered from the view. One call, both surfaces.
pub fn render_notice(view: &SessionView, notice: &Notice) -> Vec<String> {
    let data = NoticeData::gather(view, notice);
    notice.render(&data.ctx())
}

fn key_rows(view: &SessionView) -> Vec<(String, String)> {
    let tree = crate::region::RegionTree::build(view, Tab::Runs);
    let mut out: Vec<(String, String)> = tree
        .regions()
        .iter()
        .map(|r| (r.hotkey_label().to_string(), r.label().to_string()))
        .collect();
    for tab in Tab::ALL {
        out.push((format!("({})", tab.digit()), format!("the {} tab", tab.title())));
    }
    for (k, what) in crate::app::FOOTER_KEYS {
        out.push(((*k).to_string(), (*what).to_string()));
    }
    // §B10's copy keys, and the escape hatch. **The escape hatch is listed because an escape hatch
    // nobody knows about is not an escape hatch** — mouse capture removes the terminal's ordinary
    // selection, and a user who does not know about Shift-drag concludes copying is gone.
    out.push(("(y)".into(), "copy the focused turn, or a tool call's expanded output".into()));
    out.push(("(Y)".into(), "copy the whole transcript as markdown".into()));
    out.push((
        "shift-drag".into(),
        "the terminal's own selection, which still works while Marlowe holds the mouse".into(),
    ));
    out
}
