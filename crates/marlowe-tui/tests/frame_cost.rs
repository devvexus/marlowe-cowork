//! **What a frame costs.** Matthew: the screen draws at the display's refresh rate while anything
//! is live, so a live frame has to fit well inside a refresh period (6.9 ms at 144 Hz; the target
//! is under 2 ms, release build).
//!
//! A 120x40 terminal over a 200-entry transcript with maths, code, diffs and reasoning measures
//! three frames: the *cold* one (nothing cached: first draw, a resize, a theme change), the *live*
//! one (a reply streaming into the last entry, the clock ticking) and the *idle* one (nothing
//! changed). The numbers print (`cargo test --release -p marlowe-tui --test frame_cost --
//! --nocapture`). The assertions are loose, so a busy machine does not flake: the cache must make a
//! live frame less than half the cost of laying everything out again, and the live frame must stay
//! under five times its target in a release build.
//!
//! The second test is the cache's contract: a streamed session drawn with and without it is the
//! same screen, cell for cell.

mod common;

use std::time::{Duration, Instant};

use common::*;
use marlowe_proto::{Event, TurnStop};
use marlowe_tui::State;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const W: u16 = 120;
const H: u16 = 40;

const MATHS: &str = "The softmax Jacobian is\n\n$$\n\\frac{\\partial s_i}{\\partial z_j} = s_i(\\delta_{ij} - s_j)\n$$\n\nand with $s = \\mathrm{softmax}(z)$ the KL term is $\\ln\\frac{\\sigma_2}{\\sigma_1}$.\n\n```rust\nfn softmax(z: &[f32]) -> Vec<f32> {\n    let m = z.iter().cloned().fold(f32::MIN, f32::max);\n    let e: Vec<f32> = z.iter().map(|x| (x - m).exp()).collect();\n    let s: f32 = e.iter().sum();\n    e.into_iter().map(|x| x / s).collect()\n}\n```\n\n- first point with `code`\n- second point, long enough that it has to wrap onto another line at this width\n";

const DIFF: &str = "edit src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,3 +10,4 @@\n fn a() {}\n-fn b() {}\n+fn b() { println!(\"b\"); }\n+fn c() {}\n";

fn in_session() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hi");
    s.on_key(key(crossterm::event::KeyCode::Enter));
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    s
}

/// `turns` finished turns of four entries each, then one more turn that is still streaming.
fn session_with(turns: u32) -> (State, u64) {
    let mut s = in_session();
    let mut seq = 3;
    let mut f = |s: &mut State, e: Event| {
        feed(s, seq, e);
        seq += 1;
    };
    for t in 1..=turns {
        f(&mut s, Event::TurnStarted { session: sid(), turn: t, model: sonnet_ref(), user: format!("question {t}") });
        f(&mut s, Event::ReasoningDelta { session: sid(), turn: t, text: "weighing the options, then $\\alpha + \\beta$".into(), tokens: 12 });
        let call = format!("c{t}");
        f(&mut s, Event::ToolStarted { session: sid(), turn: t, call_id: call.clone(), tool: "edit".into(), summary: DIFF.into() });
        f(&mut s, Event::ToolFinished { session: sid(), turn: t, call_id: call, ok: true, summary: "ok".into() });
        f(&mut s, Event::TextDelta { session: sid(), turn: t, text: MATHS.into() });
        f(&mut s, Event::TurnFinished { session: sid(), turn: t, usage: usage(), stop: TurnStop::Done });
    }
    let t = turns + 1;
    f(&mut s, Event::TurnStarted { session: sid(), turn: t, model: sonnet_ref(), user: "and now".into() });
    f(&mut s, Event::TextDelta { session: sid(), turn: t, text: "Starting the reply, ".into() });
    (s, seq)
}

fn terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(W, H)).unwrap()
}

fn draw(t: &mut Terminal<TestBackend>, s: &State) -> Duration {
    let th = theme();
    let at = Instant::now();
    t.draw(|f| marlowe_tui::render::draw(f, s, &th)).unwrap();
    at.elapsed()
}

#[derive(Debug, Clone, Copy)]
struct Stat {
    median: Duration,
    p95: Duration,
    max: Duration,
}

fn stat(mut v: Vec<Duration>) -> Stat {
    v.sort();
    Stat { median: v[v.len() / 2], p95: v[v.len() * 95 / 100], max: *v.last().unwrap() }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn show(name: &str, s: Stat) {
    println!("{name:<28} median {:>7.3} ms   p95 {:>7.3} ms   max {:>7.3} ms", ms(s.median), ms(s.p95), ms(s.max));
}

/// A reply streaming into the last entry: every frame a few more words and a tick of the clock.
fn live_frames(mut s: State, mut seq: u64, frames: usize) -> Stat {
    let mut t = terminal();
    draw(&mut t, &s);
    let turn = s.live.as_ref().map(|l| l.turn).unwrap_or(0);
    let mut costs = Vec::new();
    for i in 0..frames {
        feed(&mut s, seq, Event::TextDelta { session: sid(), turn, text: format!("word{i} and a few more ") });
        seq += 1;
        s.now_ms += 7;
        costs.push(draw(&mut t, &s));
    }
    stat(costs)
}

/// The strip at its busiest: every frame a reasoning delta, a recall stage ticking its progress and
/// the clock, with the strip's two rows drawn under a 200-entry transcript.
fn strip_frames(mut s: State, mut seq: u64, frames: usize) -> Stat {
    use marlowe_proto::{PhaseEvent, PhaseKind};
    let mut t = terminal();
    draw(&mut t, &s);
    let turn = s.live.as_ref().map(|l| l.turn).unwrap_or(0);
    let mut costs = Vec::new();
    for i in 0..frames {
        feed(&mut s, seq, Event::ReasoningDelta { session: sid(), turn, text: format!("thought {i} "), tokens: 3 });
        seq += 1;
        let mut p = PhaseEvent::progress(PhaseKind::Recall, i as u64, frames as u64).stage("dense").unit("items");
        p.rate = Some(1.9e6);
        feed(&mut s, 0, Event::Phase(p.on(&sid(), turn)));
        s.now_ms += 7;
        costs.push(draw(&mut t, &s));
    }
    let text = dump(t.backend().buffer());
    assert!(text.contains("writing") || text.contains("thinking"), "the strip is on screen while it is measured:
{text}");
    stat(costs)
}

#[test]
fn frame_cost_cold_live_and_idle() {
    let (s, seq) = session_with(50);
    let entries = s.entries.len();
    assert!(entries >= 200, "{entries} entries");

    // Cold: nothing cached, a fresh terminal. Repeated, each time from an emptied cache.
    let mut cold = Vec::new();
    for _ in 0..12 {
        s.layout.replace(Default::default());
        cold.push(draw(&mut terminal(), &s));
    }
    let cold = stat(cold);

    // Idle: the screen is drawn again with nothing changed.
    let mut t = terminal();
    draw(&mut t, &s);
    let idle = stat((0..300).map(|_| draw(&mut t, &s)).collect());

    // Live, with the cache and without it (every entry laid out every frame).
    let live = live_frames(s.clone(), seq, 300);
    let mut scratch = s.clone();
    scratch.no_layout_cache = true;
    let uncached = live_frames(scratch, seq, 60);
    let busy = strip_frames(s.clone(), seq, 300);

    println!("\nframe cost, {W}x{H}, {entries} entries, {} build", if cfg!(debug_assertions) { "debug" } else { "release" });
    show("cold (nothing cached)", cold);
    show("live (reply streaming)", live);
    show("live, strip busy", busy);
    show("idle (nothing changed)", idle);
    show("live without the cache", uncached);
    println!("a 144 Hz refresh period is 6.944 ms; the live target is under 2 ms (release)\n");

    // The cache is what makes a live frame cheap.
    assert!(
        live.median * 2 < uncached.median,
        "a cached live frame ({:.2} ms) should be well under half than laying everything out ({:.2} ms)",
        ms(live.median),
        ms(uncached.median)
    );
    assert!(idle.median <= live.median * 2 + Duration::from_millis(1), "an unchanged frame is not dearer than a live one");
    // Loose: five times the target in a release build, and a debug build's slack on top.
    let limit = if cfg!(debug_assertions) { Duration::from_millis(60) } else { Duration::from_millis(10) };
    assert!(live.median < limit, "live frame median {:.2} ms is over {:.0} ms", ms(live.median), ms(limit));
    assert!(busy.median < limit, "live frame with the strip busy: median {:.2} ms is over {:.0} ms", ms(busy.median), ms(limit));
    if live.median > Duration::from_millis(2) && !cfg!(debug_assertions) {
        println!("note: the live frame is over its 2 ms target on this machine right now");
    }
}

#[test]
fn a_cached_session_draws_the_same_screen_as_laying_everything_out_each_frame() {
    let (mut cached, mut seq) = session_with(12);
    assert!(cached.entries.len() >= 48, "{} entries", cached.entries.len());
    let mut scratch = cached.clone();
    scratch.no_layout_cache = true;
    let (mut a, mut b) = (terminal(), terminal());
    let th = theme();
    let both = |a: &mut Terminal<TestBackend>, b: &mut Terminal<TestBackend>, cached: &State, scratch: &State, what: &str| {
        a.draw(|f| marlowe_tui::render::draw(f, cached, &th)).unwrap();
        b.draw(|f| marlowe_tui::render::draw(f, scratch, &th)).unwrap();
        assert_eq!(a.backend().buffer(), b.backend().buffer(), "the screens differ: {what}");
    };
    both(&mut a, &mut b, &cached, &scratch, "first frame");
    let turn = 13;
    let words = MATHS.split_inclusive(' ').collect::<Vec<_>>();
    for (i, w) in words.iter().cycle().take(120).enumerate() {
        for s in [&mut cached, &mut scratch] {
            feed(s, seq, Event::TextDelta { session: sid(), turn, text: (*w).to_string() });
        }
        seq += 1;
        // The reader scrolls up, down, folds a reasoning block, shows all of them again.
        match i {
            30 => {
                cached.scroll = 5;
                scratch.scroll = 5;
            }
            60 => {
                cached.scroll = 0;
                scratch.scroll = 0;
            }
            80 => {
                for s in [&mut cached, &mut scratch] {
                    s.on_key(ctrl('t'));
                }
            }
            _ => {}
        }
        both(&mut a, &mut b, &cached, &scratch, &format!("after word {i}"));
    }
    for s in [&mut cached, &mut scratch] {
        feed(s, seq, Event::TurnFinished { session: sid(), turn, usage: usage(), stop: TurnStop::Done });
    }
    both(&mut a, &mut b, &cached, &scratch, "turn finished");
    // A resize lays everything out again.
    a = Terminal::new(TestBackend::new(90, 30)).unwrap();
    b = Terminal::new(TestBackend::new(90, 30)).unwrap();
    both(&mut a, &mut b, &cached, &scratch, "after a resize");
}
