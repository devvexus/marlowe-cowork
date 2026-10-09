//! §B11's classic CLI. **The narrow, SSH, piped-stdin and no-TTY path.**
//!
//! > It is a readline REPL with **command parity, not layout parity** — every slash command, every
//! > session, every piece of data is reachable, rendered linearly. **It is not a lesser product;
//! > it is the same product without a grid.**
//!
//! Parity is structural, not a checklist: this dispatches through `crate::commands`, the same and
//! only dispatcher the TUI uses. A TUI-only command would have to be built by bypassing that
//! module, and `tests/command_parity.rs` names it if one is.
//!
//! *(v1 said "no TUI-only features". v2 amends that to no TUI-only **capabilities** — layout is
//! allowed to differ, because layout is what a grid buys.)*

use std::io::{BufRead, Write};

use marlowe_contract::text::{sanitize_line, sanitize_prose};
use marlowe_view::{ClockRead, Entry, Intent, Produce, SessionView, Tab, ToolLineState};

use crate::commands::{self, Outcome};

/// Run the REPL over any reader/writer, so a test can drive it without a terminal.
/// **The REPL is a driver, so it is handed a producer rather than making one.**
///
/// C2d: it used to call `Session::new()`, which put a concrete producer inside the render crate.
/// It now takes `&mut impl Produce`, so `marlowe-surface` can drive a producer without being able
/// to construct one -- the same property that keeps `App` honest, applied to the surface that has
/// no grid.
pub fn run(
    session: &mut impl Produce,
    input: impl BufRead,
    mut out: impl Write,
    clock: &impl ClockRead,
) -> std::io::Result<()> {
    let mut shown = 0usize;

    writeln!(
        out,
        "marlowe — classic. No grid, same commands. /help for the list."
    )?;
    drain(session, &mut out, &mut shown, clock)?;

    for line in input.lines() {
        let line = line?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }

        if let Some(rest) = text.strip_prefix('/') {
            let mut parts = rest.split_whitespace();
            let name = parts.next().unwrap_or("");
            let args: Vec<&str> = parts.collect();
            let outcome = commands::dispatch(session.view(), name, &args);
            // One renderer, both surfaces. §B11 is command parity, and parity of the WORDS is the
            // half a checklist would miss.
            let say = |out: &mut dyn Write, v: &SessionView, n: &marlowe_view::Notice| {
                for l in commands::render_notice(v, n) {
                    let _ = writeln!(out, "{l}");
                }
            };
            match outcome {
                Outcome::Quit => return Ok(()),
                Outcome::Say(n) => say(&mut out, session.view(), &n),
                Outcome::Diagnostic(lines) => {
                    for l in lines {
                        writeln!(out, "{l}")?;
                    }
                }
                // Same request path as the TUI. The producer acts; a refusal is printed by name
                // rather than swallowed, so the CLI cannot look like it worked.
                Outcome::Ask(intent) => {
                    if let Err(e) = session.apply(intent) {
                        say(&mut out, session.view(), &e.as_notice());
                    }
                    drain(session, &mut out, &mut shown, clock)?;
                }
                // Ask, then say — the same order the TUI uses, for the same reason: the summary
                // is a count of what the pane holds, and it does not hold it until the producer
                // has answered.
                Outcome::TabLive(tab, intent) => {
                    if let Err(e) = session.apply(intent) {
                        say(&mut out, session.view(), &e.as_notice());
                    }
                    say(&mut out, session.view(), &commands::pane_summary(session.view(), tab));
                    for l in commands::render_pane_linear(session.view(), tab) {
                        writeln!(out, "{l}")?;
                    }
                }
                Outcome::Tab(tab, said) => {
                    // The same rule as the TUI (§B7): the region carries the data, the transcript
                    // carries the judgment. Without a grid they arrive one after the other rather
                    // than side by side — that is the layout half of "parity, not layout parity".
                    say(&mut out, session.view(), &said);
                    for l in commands::render_pane_linear(session.view(), tab) {
                        writeln!(out, "{l}")?;
                    }
                }
                // **Parity, honestly.** §B11 is parity of *capabilities*, not of mechanisms: the
                // classic CLI has no window to open and no daemon connection to steer through, and
                // pretending otherwise would be worse than saying so. What it can do is hand over
                // the command that does it, which is §6.7's fallback and is the same thing the
                // launcher prints when it cannot open a terminal either.
                Outcome::Watch(run) => say(
                    &mut out,
                    session.view(),
                    &marlowe_view::Notice::WindowOpened {
                        run: marlowe_view::Echo::new(run.clone()),
                        terminal: None,
                        command: marlowe_view::CommandLine::new(crate::window::attach_command(&run)),
                    },
                ),
                Outcome::Rejected(r) => {
                    say(&mut out, session.view(), &marlowe_view::Notice::Refused(r))
                }
                Outcome::Unknown(name) => {
                    let nearest = commands::complete(&name).first().map(|c| c.name);
                    say(
                        &mut out,
                        session.view(),
                        &marlowe_view::Notice::Refused(marlowe_view::Refusal::UnknownCommand {
                            name: marlowe_view::Echo::new(name),
                            nearest,
                        }),
                    );
                }
            }
            shown = session.view().transcript.len();
            // A command can raise an approval too — `/state waiting` does. Surfacing it only on the
            // message path would let the classic CLI sit in `waiting` with nothing on screen to
            // answer, which is a capability gap, and §B14 forbids TUI-only capabilities.
            print_approval(session.view(), &mut out)?;
            continue;
        }

        if let Some(cmd) = text.strip_prefix('!') {
            writeln!(
                out,
                "Shell runs through the approval path, and that path lands in M2. {cmd:?} was not run."
            )?;
            continue;
        }

        // Asked, not assigned -- exactly as the TUI does it.
        if let Err(e) = session.apply(Intent::Send(text.to_string())) {
            let n = e.as_notice();
            for l in commands::render_notice(session.view(), &n) {
                writeln!(out, "{l}")?;
            }
        }
        drain(session, &mut out, &mut shown, clock)?;

        print_approval(session.view(), &mut out)?;
    }
    Ok(())
}

/// §B9 reaches the classic CLI too — approvals are a **capability**, and §B14 forbids TUI-only
/// capabilities. Linearly it is a prompt rather than an overlay, which is layout, not capability.
///
/// **States blast radius, not the command.** `BlastRadius` has no field for the command string, so
/// this cannot print one even by accident.
fn print_approval(view: &SessionView, out: &mut impl Write) -> std::io::Result<()> {
    let Some(radius) = &view.approval else {
        return Ok(());
    };
    // §B9 on the classic surface. `headline` names the verb and the scope, and the scope is a
    // model-composed argument — so these three lines are the same decision surface the TUI's
    // overlay renders, on a surface with no ratatui underneath to filter anything.
    writeln!(out, "\n  approval — {}", sanitize_line(&radius.headline()))?;
    writeln!(out, "  {}", sanitize_line(&radius.consequence()))?;
    writeln!(out, "  {}", sanitize_line(&radius.why()))?;
    let keys: Vec<String> = radius
        .keys()
        .iter()
        .map(|(k, what)| {
            let key = match k {
                '\n' => "enter".to_string(),
                '\u{1b}' => "esc".to_string(),
                c => c.to_string(),
            };
            format!("{key} {what}")
        })
        .collect();
    writeln!(out, "  {}\n", keys.join(" · "))
}

/// Play out every pending beat and print what appeared.
///
/// The TUI animates live tool lines in place; without a grid there is nowhere to animate, so the
/// linear surface prints each line once it has settled. Same data, same order, no cursor tricks.
fn drain(
    session: &mut impl Produce,
    out: &mut impl Write,
    shown: &mut usize,
    clock: &impl ClockRead,
) -> std::io::Result<()> {
    let start = clock.now_ms();
    let mut t = start;
    loop {
        session.tick(t);
        print_new(session.view(), out, shown)?;
        match session.next_beat_in(t) {
            Some(dt) => t += dt.max(1),
            None => break,
        }
        // The scripted beats are bounded; this only guards against a future script that is not.
        if t - start > 60_000 {
            break;
        }
    }
    session.tick(t);
    print_new(session.view(), out, shown)
}

fn print_new(view: &SessionView, out: &mut impl Write, shown: &mut usize) -> std::io::Result<()> {
    while *shown < view.transcript.len() {
        match &view.transcript[*shown] {
            Entry::User(t) => writeln!(out, "> {}", sanitize_prose(t))?,
            Entry::Said(t) => match t {
                // The classic surface renders both halves identically -- a user must not see a
                // seam between the model talking and the harness talking.
                marlowe_view::Speech::Model(text) => writeln!(out, "{}", sanitize_prose(text))?,
                // Harness-authored, but a notice interpolates targets and details that are not,
                // and "harness-authored today" is an assumption about every future notice. The
                // quarantine's own carve-out for harness-authored bodies was written and then
                // removed for exactly this reason; the rule is unconditional here too.
                marlowe_view::Speech::Harness(n) => {
                    for line in crate::commands::render_notice(view, n) {
                        writeln!(out, "{}", sanitize_line(&line))?;
                    }
                }
            },
            Entry::Compacted { turns } => writeln!(out, "-- compacted · {turns} turns → summary")?,
            // Linear surfaces have nowhere to collapse to, so the classic CLI reports the shape
            // rather than the content: it is progress, not an answer.
            Entry::Reasoning { tokens, done, .. } => writeln!(
                out,
                "  {} thinking{}{}",
                crate::chrome::TOOL_MARKER,
                match tokens {
                    // Missing rather than zero — see the head line in `render.rs`.
                    0 => String::new(),
                    1 => "   1 token".to_string(),
                    n => format!("   {n} tokens"),
                },
                if *done { "" } else { " …" }
            )?,
            Entry::Tools(calls) => {
                for c in calls {
                    let target = if c.collapsed.is_empty() {
                        c.target.clone()
                    } else {
                        format!("{} files", c.collapsed.len() + 1)
                    };
                    let right = match (&c.summary_line, &c.state) {
                        // Rendered upstream: the daemon's fold cannot rebuild typed metrics from
                        // a string, so it carries the rendered line. Preferred where it is set.
                        (Some(line), _) => line.clone(),
                        (None, ToolLineState::Running { elapsed_ms }) => {
                            format!("{elapsed_ms} ms")
                        }
                        (None, ToolLineState::Ok(s) | ToolLineState::Failed(s)) => s.render(),
                    };
                    // Every field on a §B6 tool line is Line-shaped: a `\n` in `target` forges a
                    // second tool line, which reads as a call the model never made.
                    writeln!(
                        out,
                        "  ... {:<9} {:<40} {}",
                        sanitize_line(&c.verb),
                        sanitize_line(&target),
                        sanitize_line(&right)
                    )?;
                    if c.expanded {
                        for t in &c.collapsed {
                            writeln!(out, "        {}", sanitize_line(t))?;
                        }
                        if let ToolLineState::Ok(s) | ToolLineState::Failed(s) = &c.state {
                            if let Some(d) = &s.detail {
                                for l in d.lines() {
                                    writeln!(out, "        {}", sanitize_line(l))?;
                                }
                            }
                        }
                    }
                }
            }
        }
        *shown += 1;
    }
    out.flush()
}

/// Print one pane linearly. Used by `--classic --print <tab>` for a piped, no-TTY read.
pub fn print_pane(view: &SessionView, tab: Tab, mut out: impl Write) -> std::io::Result<()> {
    for l in commands::render_pane_linear(view, tab) {
        writeln!(out, "{l}")?;
    }
    Ok(())
}

/// **The display sanitiser on the classic surface.**
///
/// A separate named test from the two in `marlowe/src/agent.rs`, because reverting the sanitiser
/// *here* must fail a test that names *this* site. The classic CLI has no ratatui underneath it —
/// it writes bytes straight to a terminal — so this is the surface where an escape sequence has
/// nothing at all standing between the model and the screen.
#[cfg(test)]
mod display_sanitiser {
    use super::*;
    use marlowe_view::notice::Speech;
    use marlowe_view::model::ToolCall;
    use marlowe_view::turn::ToolLineState;

    const OVERWRITE: &str = "\u{1b}[2K\r";

    fn view_with(entries: Vec<Entry>) -> SessionView {
        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.transcript = entries;
        view
    }

    fn printed(view: &SessionView) -> String {
        let mut out: Vec<u8> = Vec::new();
        let mut shown = 0usize;
        print_new(view, &mut out, &mut shown).unwrap();
        String::from_utf8(out).expect("the classic surface writes utf-8")
    }

    #[test]
    fn the_classic_surface_never_writes_a_control_sequence_to_the_terminal() {
        let s = printed(&view_with(vec![
            Entry::User(format!("hello{OVERWRITE}")),
            Entry::Said(Speech::Model(format!("an answer{OVERWRITE}forged"))),
            Entry::Tools(vec![ToolCall {
                id: 1,
                verb: "bash",
                target: format!("rm -rf /{OVERWRITE}ls"),
                state: ToolLineState::Running { elapsed_ms: 12 },
                collapsed: Vec::new(),
                summary_line: None,
                expanded: false,
            }]),
        ]));

        assert!(!s.contains('\u{1b}'), "ESC reached the terminal:\n{s:?}");
        assert!(!s.contains('\r'), "CR reached the terminal:\n{s:?}");
        assert!(s.contains("<U+001B>"), "stripped silently instead of marked:\n{s:?}");

        // **The vacuity control.** Every assertion above passes on an empty string, so this
        // asserts the entries actually reached the writer.
        assert!(s.contains("hello"), "nothing was printed at all:\n{s:?}");
        assert!(s.contains("an answer"), "{s:?}");
        assert!(s.contains("rm -rf /"), "{s:?}");
    }

    #[test]
    fn a_tool_target_cannot_forge_a_second_b6_line() {
        // §B6 is one line per call. A `\n` in a target invents a call that never happened, and on
        // a linear surface there is no grid geometry to make the forgery obvious.
        let s = printed(&view_with(vec![Entry::Tools(vec![ToolCall {
            id: 1,
            verb: "read",
            target: "a.txt\n  ... bash      rm -rf /".into(),
            state: ToolLineState::Running { elapsed_ms: 1 },
            collapsed: Vec::new(),
            summary_line: None,
            expanded: false,
        }])]));
        assert_eq!(s.lines().count(), 1, "the target forged a second tool line:\n{s}");
        assert!(s.contains("<U+000A>"), "the newline was dropped, not marked:\n{s}");
    }

    /// **This test exists because the mutation run found nothing.**
    ///
    /// Reverting the sanitiser in `print_approval` failed **zero** tests, while the three tests
    /// above all covered `print_new`. Two sites, one guarded, and a green suite either way — which
    /// is the fourteenth-instance family in miniature: a guard whose subject nothing checks.
    ///
    /// It would not have been found by reading, because the sanitiser calls are visibly *there* in
    /// `print_approval`. Only reverting them one site at a time says whether anything notices.
    ///
    /// `headline()` is the line that carries attacker-influenced text — `within` is a path the
    /// model named, `recipient` an address it chose — while `consequence()` is a fixed string.
    #[test]
    fn the_classic_approval_prompt_never_writes_a_control_sequence() {
        use marlowe_view::approval::{
            BlastRadius, Ceiling, Effect, Novelty, Offered, PathLabel, RiskTier,
        };

        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.approval = Some(BlastRadius {
            effect: Effect::Execute {
                touches: 4,
                within: PathLabel::new(format!("/work/src{OVERWRITE}/etc")),
                reaches_network: true,
            },
            tier: RiskTier::Irreversible,
            novelty: Novelty::Routine,
            ceiling: Ceiling::AtCeiling,
            offered: Offered { edit_first: false, send_as_marlowe: false },
        });

        let mut out: Vec<u8> = Vec::new();
        print_approval(&view, &mut out).unwrap();
        let s = String::from_utf8(out).expect("utf-8");

        assert!(!s.contains('\u{1b}'), "ESC reached the approval prompt:\n{s:?}");
        assert!(!s.contains('\r'), "CR reached the approval prompt:\n{s:?}");
        assert!(s.contains("<U+001B>"), "stripped silently instead of marked:\n{s:?}");
        // Vacuity control: the prompt rendered at all, and still names what is being approved.
        assert!(s.contains("/work/src"), "the approval never rendered:\n{s:?}");
        assert!(s.contains("approval"), "{s:?}");
    }

    #[test]
    fn a_model_reply_keeps_its_paragraphs() {
        // The shape distinction where it is applied: prose is prose. A sanitiser that flattened
        // model replies would pass every assertion above and make the product worse.
        let s = printed(&view_with(vec![Entry::Said(Speech::Model(
            "first line\nsecond line".into(),
        ))]));
        assert_eq!(s, "first line\nsecond line\n");
    }
}
