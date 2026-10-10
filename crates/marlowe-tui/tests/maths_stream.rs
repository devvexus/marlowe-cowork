//! **Maths in the real pane, streamed token by token through the reducer.**
//!
//! `marlowe-surface/tests/maths_corpus.rs` proves the markdown layer. This proves the wiring: the
//! entry still being written is rendered live (an unclosed span is held, never shown as source),
//! the finished entry is not, and the user's own maths is typeset too.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{Event, TurnStop};

/// A reply as a model writes one, cut into token-sized pieces.
const REPLY: &str = "The update is $\\theta \\leftarrow \\theta - \\eta \\nabla_\\theta \\mathcal{L}$, where\n\
$$\n\
\\mathcal{L} = -\\frac{1}{N}\\sum_{i=1}^{N} \\log p_\\theta(y_i \\mid x_i)\n\
$$\n\
and the rotation is\n\
\\[\n\
R = \\begin{pmatrix} \\cos\\theta & -\\sin\\theta \\\\ \\sin\\theta & \\cos\\theta \\end{pmatrix}\n\
\\]\n\
so $\\det R = 1$. Done.";

fn tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(3).map(|c| c.iter().collect()).collect()
}

/// A session with one turn started, as the reducer tests open one. `user` is what was typed.
fn in_session(user: &str) -> marlowe_tui::State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    // What is typed only opens the session; the turn's `user` is what the transcript shows.
    type_str(&mut s, "hi");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 3, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: user.into() });
    s
}

/// Rows of the screen, so a landed block can be checked for movement.
fn rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    dump(buf).lines().map(str::to_string).collect()
}

#[test]
fn a_streamed_reply_never_shows_source_and_a_landed_block_never_moves() {
    // Tall enough that nothing scrolls, so a row's position on screen is its position in the reply.
    let (w, h) = (W, 90);
    let mut s = in_session("explain");
    let mut prev: Vec<String> = rows(&render(&s, w, h));
    let mut so_far = String::new();
    let mut landed_at: Option<usize> = None;
    let mut frames = 0;
    for (k, tok) in tokens(REPLY).into_iter().enumerate() {
        so_far.push_str(&tok);
        feed(&mut s, 4 + k as u64, Event::TextDelta { session: sid(), turn: 1, text: tok });
        let buf = render(&s, w, h);
        let now = rows(&buf);
        let screen = now.join("\n");
        frames += 1;
        assert!(!screen.contains('\\'), "frame {k} showed a backslash:\n{screen}");
        assert!(!screen.contains('$'), "frame {k} showed a dollar:\n{screen}");
        assert_palette_only(&buf);
        // The first display block has landed once its closing `$$` has arrived.
        if landed_at.is_none() && so_far.matches("$$").count() >= 2 {
            let last = now.iter().rposition(|l| l.contains('⎯')).expect("the fraction bar is on screen");
            landed_at = Some(last + 1);
        }
        if let Some(n) = landed_at {
            // Everything from the top of the screen to the end of the landed block is fixed.
            assert_eq!(&now[..n], &prev[..n], "frame {k} moved a row at or above a landed display block");
        }
        prev = now;
    }
    assert!(landed_at.is_some(), "no display block landed");
    let live = prev.clone();
    feed(&mut s, 10_000, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
    let done = rows(&render(&s, w, h));
    let n = landed_at.unwrap();
    // The rows down to the Conversation box's top border carry the title and the activity meter,
    // which change when the turn ends. The reply starts below it.
    let top = done.iter().position(|l| l.contains("╭ Conversation")).map_or(1, |k| k + 1);
    assert_eq!(&done[top..n], &live[top..n], "finishing the turn re-laid out the reply");
    let screen = done.join("\n");
    for want in ["θ ← θ − η∇_θℒ", "⎛cos θ", "det R = 1", "Done."] {
        assert!(screen.contains(want), "{want:?} is not on screen:\n{screen}");
    }
    println!("tui maths stream: {frames} frames, 0 raw, landed block fixed from row {n}");
}

#[test]
fn the_users_own_maths_is_typeset_and_the_rest_left_as_typed() {
    let user = "why is **this** $\\frac{a}{b} \\leq \\sqrt{c}$ and not $5 and $10?";
    let s = in_session(user);
    let screen = dump(&render(&s, W, H));
    assert!(screen.contains("a/b ≤ √c"), "{screen}");
    assert!(screen.contains("**this**"), "the user's markdown was interpreted:\n{screen}");
    assert!(screen.contains("$5 and $10?"), "currency was read as maths:\n{screen}");
    assert!(!screen.contains("\\frac"), "{screen}");
}
