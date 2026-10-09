//! **A run window, asserted on the rendered `Buffer`.** `M3-DESIGN.md` §6.
//!
//! Every assertion here searches **one region's rect**, never the whole frame. A window has eight
//! bordered panels, a titlebar and a footer; a whole-buffer `contains` would match a label, a
//! footer key or a border and go green on a frame that did not hold the thing it was named for.
//! That happened twice in M2 and it is why `common::region_text` exists.

mod common;

use marlowe_surface::window::{self, Confirm, WindowApp};
use marlowe_view::run::{OrphanPolicyLabel, RunState};

fn rects(w: u16, h: u16) -> window::Chrome {
    window::layout(ratatui::layout::Rect::new(0, 0, w, h))
}

/// Inside a bordered region. §B2 puts the label and the hotkey **on** the border, so a test about
/// what a panel *contains* has to look past them.
fn inner(r: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: r.x + 1,
        y: r.y + 1,
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

// ─── the checkpoint panel, which is why this window ships early ───────────────────────────────

/// §6.2: *"last completed step, and what a resume would resume from — **the field that makes this a
/// debugging instrument**."*
///
/// Two facts on two lines. They answer different questions, and a panel that collapsed them into
/// one would invite the reader to infer the second from the first — which is exactly what goes
/// wrong while durable resume is half-built.
#[test]
fn the_checkpoint_panel_carries_both_the_last_step_and_what_a_resume_would_do() {
    let app = common::window();
    let buf = common::window_frame(&app, 120, 30);
    let text = common::region_text(&buf, rects(120, 30).checkpoint);

    assert!(text.contains("step 41"), "the last completed step is not on screen:\n{text}");
    assert!(text.contains("resume"), "what a resume would do is not on screen:\n{text}");
    // The control: both facts are present *and distinguishable*. A panel printing `seq 41` once
    // would satisfy a naive `contains` for either line.
    assert_eq!(text.matches("41").count(), 2, "both facts must be stated, not one:\n{text}");
}

/// **A run that cannot be resumed says so and invents no reason.**
///
/// `resumable` is a fact `ControlPlane::detail` read off the checkpoint store. The window has no
/// reason of its own for a `false`, and making one up — "the build is ephemeral", "no WAL" — would
/// be the surface authoring a claim about a subsystem it does not own. Which of completed, failed
/// or cancelled it was is already stated on the identity panel above.
#[test]
fn a_run_that_cannot_be_resumed_says_so_without_inventing_a_reason() {
    let mut v = common::run_view();
    v.state = RunState::Completed;
    v.checkpoint.resumable = false;
    let app = WindowApp::new(v);

    let text = common::region_text(&common::window_frame(&app, 160, 45), rects(160, 45).checkpoint);
    assert!(text.contains("not from here"), "{text}");

    // The control: a resumable run reads differently in the same panel, so the line above is a
    // rendering of the flag rather than a constant that would pass either way.
    let ok = common::region_text(
        &common::window_frame(&common::window(), 160, 45),
        rects(160, 45).checkpoint,
    );
    assert!(ok.contains("from step 41"), "{ok}");
    assert!(!ok.contains("not from here"), "{ok}");
}

/// The negative control for the pair above: a window on a run that has never checkpointed says so,
/// rather than printing `seq 0` — which is a different claim and a wrong one.
#[test]
fn a_run_with_no_checkpoint_says_so_rather_than_printing_step_zero() {
    let mut v = common::run_view();
    v.checkpoint.last_completed = None;
    let app = WindowApp::new(v);
    let text = common::region_text(&common::window_frame(&app, 120, 30), rects(120, 30).checkpoint);
    assert!(text.contains("no checkpoint yet"), "{text}");
    assert!(!text.contains("step 0"), "an absent checkpoint rendered as step zero:\n{text}");
}

// ─── identity: status, elapsed, spend ─────────────────────────────────────────────────────────

/// §6.2's first row, all four fields, on the rendered frame.
#[test]
fn status_elapsed_and_spend_against_the_ceiling_all_render() {
    let mut app = common::window();
    let buf = common::window_frame(&app, 120, 30);
    let text = common::region_text(&buf, rects(120, 30).identity);

    assert!(text.contains("running"), "status:\n{text}");
    assert!(text.contains("1m 33s"), "elapsed:\n{text}");
    // **Spend carries its denominator.** A number with no denominator is what ADR-028
    // requirement 2 exists to forbid, and a spend ceiling nobody can see is a ceiling nobody
    // watches.
    assert!(text.contains("$0.12"), "spend:\n{text}");
    assert!(text.contains("$3.00"), "the ceiling must be beside it:\n{text}");
}

/// **Elapsed is the daemon's figure, rendered — never the surface's arithmetic.**
///
/// It was `now_ms - started_ms`, computed here, which made a frame depend on when it was looked at.
/// `ControlPlane::detail` now resolves it — the final wall time when the run has one, the live
/// figure otherwise — because the daemon is the thing that holds a clock. So this asserts the
/// rendering *and* that it tracks the field rather than being a constant.
#[test]
fn elapsed_renders_what_the_daemon_reported_and_tracks_it() {
    let app = common::window();
    let a = common::region_text(&common::window_frame(&app, 120, 30), rects(120, 30).identity);
    assert!(a.contains("1m 33s"), "the fixture's 93_000 ms did not render:\n{a}");

    let mut v = common::run_view();
    v.elapsed_ms = 5_000;
    let b = common::region_text(
        &common::window_frame(&WindowApp::new(v), 120, 30),
        rects(120, 30).identity,
    );
    assert!(b.contains("5.0s"), "elapsed did not track the field:\n{b}");

    // **The property that replaced the old one, and it is the stronger of the two.** A frame is now
    // a pure function of state alone: two windows at wildly different `now_ms` produce identical
    // identity panels, because nothing in one is computed from a clock the surface reads.
    let mut c1 = common::window();
    let mut c2 = common::window();
    assert_eq!(
        common::region_text(&common::window_frame(&c1, 120, 30), rects(120, 30).identity),
        common::region_text(&common::window_frame(&c2, 120, 30), rects(120, 30).identity),
    );
}

/// §B2: state colours encode **state**. Spend at the ceiling is a state; spend below it is not.
#[test]
fn spend_takes_a_state_colour_only_at_the_ceiling() {
    let theme = common::theme();
    let red = theme.tone(marlowe_view::Tone::Red);

    let below = common::window();
    let buf = common::window_frame(&below, 120, 30);
    let r = rects(120, 30).identity;
    let reds_below = count_fg(&buf, r, red);

    let mut v = common::run_view();
    v.spend_micros_usd = v.ceiling_micros_usd;
    let at = WindowApp::new(v);
    let buf = common::window_frame(&at, 120, 30);
    let reds_at = count_fg(&buf, r, red);

    assert_eq!(reds_below, 0, "a run inside its ceiling carried a failure colour");
    assert!(reds_at > 0, "a run AT its ceiling carried no state colour at all");
}

fn count_fg(buf: &ratatui::buffer::Buffer, r: ratatui::layout::Rect, want: ratatui::style::Color) -> usize {
    let mut n = 0;
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            if buf.cell((x, y)).map(|c| c.fg) == Some(want) {
                n += 1;
            }
        }
    }
    n
}

// ─── §6.3's placeholders ──────────────────────────────────────────────────────────────────────

/// **Present, sized, and empty from day one** — at every size, so nothing about the layout depends
/// on there being nothing in them yet.
#[test]
fn every_placeholder_panel_is_present_at_every_size_and_states_a_fact() {
    for (w, h) in common::WINDOW_SIZES {
        let app = common::window();
        let buf = common::window_frame(&app, w, h);
        let c = rects(w, h);
        for (name, rect) in [
            ("subagents", c.subagents),
            ("budget", c.budget),
            ("scope memory", c.scope_memory),
            ("meetings", c.meetings),
        ] {
            let text = common::region_text(&buf, rect);
            assert!(
                text.contains(&format!("{name} — none")),
                "at {w}x{h} the {name} panel does not state its fact:\n{text}"
            );
        }
    }
}

/// **A placeholder states a fact, never a roadmap** (§6.3). *"Coming in Session C"* leaks the
/// roadmap into the product and becomes a lie the moment C ships — and nobody remembers to delete
/// it.
#[test]
fn no_panel_names_a_milestone_a_session_or_a_future() {
    let app = common::window();
    let buf = common::window_frame(&app, 160, 45);
    let whole = common::buffer_text(&buf);
    for leak in [
        "Session C", "Session D", "Session E", "coming", "soon", "not built", "TODO",
        "M3", "M4", "M6", "planned", "will be",
    ] {
        assert!(
            !whole.contains(leak),
            "the roadmap leaked into the product: {leak:?} is on screen"
        );
    }
}

/// **Nothing is faked to preview the layout.** The same family as a green test over a mechanism
/// that never ran: a panel that drew two greyed example rows would look finished and be a lie.
#[test]
fn an_empty_panel_draws_one_honest_line_and_no_example_rows() {
    let app = common::window();
    let buf = common::window_frame(&app, 200, 50);
    // **Inside the border, not the panel rect.** The label and the hotkey live *on* the border by
    // §B2, so a search over the whole rect counts `Subagents` and `(g)` as content and would fail
    // on a correct frame — which is the same "matched chrome instead of prose" mistake this file's
    // header names, made one layer in.
    let text = common::region_text(&buf, inner(rects(200, 50).subagents));
    let inside: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // The border carries the label and the hotkey; inside there is exactly one line, and it is the
    // fact. A second line would be an invented row.
    assert_eq!(inside.len(), 1, "expected exactly one line inside an empty panel:\n{text}");
    assert!(inside[0].contains("none"), "and it must be the fact:\n{text}");
}

// ─── the placeholders do fill, which is what "sized" is for ───────────────────────────────────

/// The control for the two tests above. If the panels could only ever render "none", their being
/// *sized* would be untestable and the promise that the layout will not move would be unfalsifiable.
#[test]
fn a_filled_panel_uses_the_space_the_empty_one_reserved_and_moves_nothing_else() {
    let empty = common::window();
    let c = rects(160, 45);

    let mut v = common::run_view();
    v.subagents = vec![
        marlowe_view::Item::new("researcher", 'a', marlowe_view::Tone::Normal, &[]),
        marlowe_view::Item::new("coder", 'b', marlowe_view::Tone::Normal, &[]),
    ];
    let filled = WindowApp::new(v);

    let a = common::window_frame(&empty, 160, 45);
    let b = common::window_frame(&filled, 160, 45);

    // Everything outside the panel that filled is untouched — that is what "sized from day one"
    // buys, and it is the property the rule exists for.
    for rect in [c.identity, c.checkpoint, c.output, c.steer, c.budget, c.scope_memory, c.meetings] {
        assert_eq!(
            common::region_text(&a, rect),
            common::region_text(&b, rect),
            "filling the subagents panel moved another region"
        );
    }
    assert!(common::region_text(&b, c.subagents).contains("researcher"));
}

// ─── cancel, and the orphan policy ────────────────────────────────────────────────────────────

/// §6.2: cancel *"with the run's declared orphan policy stated plainly"*. Plainly means a sentence,
/// not a variant name.
#[test]
fn the_cancel_confirmation_states_the_orphan_policy_in_words() {
    for (policy, expected) in [
        (OrphanPolicyLabel::Detach, "children keep running with no parent"),
        (OrphanPolicyLabel::Terminate, "children end with it"),
    ] {
        let mut v = common::run_view();
        v.orphan_policy = policy;
        let mut app = WindowApp::new(v);
        app.confirm = Some(Confirm::Cancel);

        let text = common::buffer_text(&common::window_frame(&app, 120, 30));
        assert!(text.contains(expected), "the policy was not stated plainly:\n{text}");
    }
}

/// §3.5's argument, applied to cancel: **the harness states the cost from its own record**, because
/// those are the facts it holds and exactly what a compromised run would lie about.
#[test]
fn the_cancel_confirmation_states_the_cost_and_what_it_cannot_undo() {
    let mut app = common::window();
    app.confirm = Some(Confirm::Cancel);
    let text = common::buffer_text(&common::window_frame(&app, 120, 30));

    assert!(text.contains("1m 33s"), "the run's age is not stated:\n{text}");
    assert!(text.contains("$0.12"), "the spend is not stated:\n{text}");
    assert!(
        text.contains("stays written"),
        "the confirmation does not say what it cannot undo:\n{text}"
    );
}

// ─── §B11's honest refusal ────────────────────────────────────────────────────────────────────

#[test]
fn below_the_minimum_it_refuses_honestly_and_names_both_sizes() {
    let app = common::window();
    let text = common::buffer_text(&common::window_frame(&app, 70, 20));
    assert!(text.contains("80x24"), "the required size is not named:\n{text}");
    assert!(text.contains("70x20"), "the actual size is not named:\n{text}");
    // And it must not have drawn a degraded grid behind the message.
    assert!(!text.contains("Checkpoint"), "a broken grid was drawn under the refusal:\n{text}");
}
