//! Not an acceptance row — a way to look at the screen.
//!
//! Run with `cargo test -p marlowe-surface --test snapshot -- --nocapture` to print the frame as
//! text. Tests can pass while the screen looks wrong, and this is the cheapest defence against
//! that. It asserts only the handful of things that would mean the layout collapsed.

mod common;

use marlowe_surface::app::{App, Key};
use marlowe_view::{StatusState, Tab};

fn show(name: &str, app: &App) {
    println!("\n=== {name} ===");
    println!("{}", common::buffer_text(&common::frame(app, 120, 30)));
}

#[test]
fn the_frame_renders_and_can_be_read() {
    let mut r = common::rig();
    r.tick(300);
    show("default — schedule tab, listening", &r.app);

    let mut runs = common::rig();
    runs.app.tab = Tab::Runs;
    runs.app.focus = marlowe_surface::region::RegionId::Item(Tab::Runs.into(), 4);
    runs.force_state(StatusState::Running, 41_000);
    show("runs tab, steer focused, running", &runs.app);

    let mut waiting = common::rig();
    waiting.force_state(StatusState::Waiting, 0);
    show("approval overlay — the only element that dims the frame", &waiting.app);

    let mut trust = common::rig();
    trust.app.tab = Tab::Trust;
    show("trust tab — present, reachable, and honest about M2", &trust.app);

    let mut typing = common::rig();
    typing.app.input = "/s".into();
    show("slash autocomplete", &typing.app);

    let mut small = common::rig();
    small.tick(0);
    println!("\n=== 100x24 — honest refusal, never a degraded grid ===");
    println!("{}", common::buffer_text(&common::frame(&small.app, 100, 24)));

    // The things that would mean it collapsed.
    let text = common::buffer_text(&common::frame(&common::app(), 120, 30));
    for want in [
        "Model", "Profile", "Session", "Workspace", "Autonomy", "Status", "Conversation",
                // ADR-056: the footer's Voice chord moved to `alt-v` so `Ctrl-V` could be paste.
        "Message", "(m)", "(v)", "(c)", "(i)", "alt-v", "turn 12", "Schedule",
    ] {
        assert!(text.contains(want), "the frame is missing {want:?}");
    }
}

#[test]
fn a_dropdown_draws_over_the_frame_without_a_fill() {
    let mut r = common::rig();
    r.key(Key::Esc, 0);
    r.key(Key::Char('a'), 0);
    r.key(Key::Enter, 0);
    show("autonomy dropdown open", &r.app);
    let text = common::buffer_text(&common::frame(&r.app, 120, 30));
    assert!(text.contains("observe") && text.contains("act"));
    // §B14: no background fill to signal selection. The marker carries it.
    assert!(text.contains('›'));
}

/// ADR-047. **Look at a markdown reply.** Tests can pass while the screen reads badly, and this is
/// the cheapest defence against that — the same argument this file's header already makes.
///
/// It asserts only the handful of things that would mean the renderer collapsed; the value is in
/// running it with `--nocapture` and reading the pane.
#[test]
fn a_markdown_reply_can_be_read() {
    use marlowe_view::notice::Speech;
    use marlowe_view::Entry;

    let reply = concat!(
        "# Cost base, June\n\n",
        "The middle band was priced against the **old egress rate**, and that went up *eleven\n",
        "percent* in June. Three consequences:\n\n",
        "1. the sheet as drafted loses money at volume\n",
        "2. `pricing/bands.csv` needs the new rate before it goes out\n",
        "   - the old figure is in row 14\n",
        "   - `fetch.py` pins the digest, so a re-run is safe\n",
        "3. ~~re-open the negotiation~~ — not needed if we flag it\n\n",
        "> Filtering does not work. Containment works.\n\n",
        "| band | old | new |\n|---|---|---|\n| low | 0.12 | 0.12 |\n| middle | 0.31 | 0.34 |\n\n",
        "```rust\nlet rate = bands.get(\"middle\").expect(\"present\");\n```\n\n",
        "Margin is $\\alpha \\times \\beta^2$ at volume; the tail integral\n",
        "$\\hat{x}$ is left as written. See [the note](https://example.invalid/j).\n\n",
        "---\n\n",
        "I've left the bands as agreed and added a line saying the middle band is under review.\n",
    );

    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::User("did anything change since".into()));
    view.transcript
        .push(Entry::Said(Speech::Model(reply.to_string())));
    let mut app = App::new(view).expect("the shipped key set has no conflicts");
    app.scroll = Some(0);
    println!("\n=== markdown reply, 120x30, top of the pane ===");
    println!("{}", common::buffer_text(&common::frame(&app, 120, 30)));
    println!("\n=== the same reply at 160x45 ===");
    println!("{}", common::buffer_text(&common::frame(&app, 160, 45)));

    let text = common::buffer_text(&common::frame(&app, 160, 45));
    assert!(text.contains("Cost base, June"), "the heading is gone");
    assert!(!text.contains("**"), "source markers reached the screen");
    assert!(text.contains("• "), "the nested list lost its bullet");
    assert!(text.contains("β²"), "the maths that CAN be shown was not");
    assert!(
        text.contains("$\\hat{x}$"),
        "the maths that cannot be shown must stay visibly its source"
    );
}
