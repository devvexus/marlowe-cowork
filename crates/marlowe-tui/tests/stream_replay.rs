//! **A streamed maths reply replayed through the real pane, frame by frame.**
//!
//! Matthew's first hands-on (ui-preview5, dusk-27b answering "the softmax Jacobian as a matrix and
//! the KL divergence between two Gaussians"): the view scrolled up and down hard while it printed.
//! `maths_stream.rs` could never see that: it renders 90 rows tall so nothing scrolls. This
//! replays a scripted reply of the same shape, token by token, at a real terminal size, and records
//! per frame what the reader sees: the first transcript line on screen (`start`), the transcript's
//! length, and the screen rows.
//!
//! Following the tail, `start` may only grow (new text pushes the view down a line at a time).
//! Every frame where it shrinks is the whole screen jumping back. Scrolled up, the rows on screen
//! must not change at all.
//!
//! What it found, and what fixed it:
//! - A held display block reserves the rows of whatever its partial source parses to, and a
//!   partial ending in `\frac`, `\left`, `\end`, `\text`… repairs or paraphrases to fewer rows
//!   than the token before it. The tail shrinks and grows by 1–3 rows on consecutive tokens.
//! - A token that ends in `\n` inside a multi-line display reads as a blank line, so for that one
//!   frame the unfinished formula is drawn as finished, then held again on the next token.
//! - Scrolled up, `State::scroll` counts lines from the bottom, so every new line moves the
//!   reader's rows.
//!
//! The fix: a held display is one placeholder row whatever has arrived, a trailing newline keeps
//! it held, and the pane keeps its top line (`render/conversation.rs`, `view_top`) whether it is
//! following the tail or scrolled up.
//!
//! `MARLOWE_TRACE=<file>` writes every frame's numbers there; `MARLOWE_SHOW=<frame>` prints that
//! frame's pane and the one before it; the jumps always print.

mod common;

use std::fmt::Write as _;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{Event, TurnStop};
use marlowe_tui::State;
use ratatui::layout::Rect;

/// A reply shaped like dusk's: headings, inline maths, a fraction, a wide matrix with
/// `\operatorname` after it, a multi-line `\[` matrix, and the KL formula with an underbrace.
const REPLY: &str = r"## Softmax Jacobian

For $s = \mathrm{softmax}(z)$ with $s_i = \frac{e^{z_i}}{\sum_k e^{z_k}}$, the derivative of each output with respect to each input is

$$
\frac{\partial s_i}{\partial z_j} = s_i(\delta_{ij} - s_j)
$$

so as a matrix

$$
J = \begin{pmatrix} s_1(1-s_1) & -s_1 s_2 & \cdots & -s_1 s_n \\ -s_2 s_1 & s_2(1-s_2) & \cdots & -s_2 s_n \\ \vdots & \vdots & \ddots & \vdots \\ -s_n s_1 & -s_n s_2 & \cdots & s_n(1-s_n) \end{pmatrix} = \operatorname{diag}(s) - s s^T
$$

For three classes it is

\[
J = \begin{bmatrix}
s_1(1-s_1) & -s_1 s_2 & -s_1 s_3 \\
-s_2 s_1 & s_2(1-s_2) & -s_2 s_3 \\
-s_3 s_1 & -s_3 s_2 & s_3(1-s_3)
\end{bmatrix}
\]

## KL Divergence Between Two Gaussians

For $p = \mathcal{N}(\mu_1, \Sigma_1)$ and $q = \mathcal{N}(\mu_2, \Sigma_2)$ on $d$ dimensions:

$$
D_{KL}(p \| q) = \frac{1}{2}\left[ \underbrace{\operatorname{tr}(\Sigma_2^{-1}\Sigma_1)}_{\text{trace term}} + (\mu_2-\mu_1)^T \Sigma_2^{-1} (\mu_2-\mu_1) - d + \ln\frac{\det\Sigma_2}{\det\Sigma_1} \right]
$$

In one dimension this is $\ln\frac{\sigma_2}{\sigma_1} + \frac{\sigma_1^2 + (\mu_1-\mu_2)^2}{2\sigma_2^2} - \frac{1}{2}$, which is zero exactly when the two distributions are the same.";

/// Tokens the way a BPE model emits them: a command with its backslash, a word with the space
/// before it, every other character alone.
fn tokens(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let start = i;
        if cs[i] == '\\' && cs.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
            while i < cs.len() && cs[i].is_ascii_alphabetic() {
                i += 1;
            }
        } else if cs[i] == ' ' && cs.get(i + 1).is_some_and(|c| c.is_alphabetic()) {
            i += 1;
            while i < cs.len() && cs[i].is_alphabetic() {
                i += 1;
            }
        } else if cs[i].is_alphabetic() {
            while i < cs.len() && cs[i].is_alphabetic() {
                i += 1;
            }
        } else {
            i += 1;
        }
        out.push(cs[start..i].iter().collect());
    }
    out
}

fn in_session() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hi");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    // An earlier exchange, so the transcript is taller than the pane before the reply starts and
    // every frame of it is followed at the tail, as in a real session.
    feed(&mut s, 3, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "hi".into() });
    let earlier = "An earlier answer, long enough to fill the pane on its own. ".repeat(6);
    let earlier = vec![earlier; 8].join("\n\n");
    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: earlier });
    feed(&mut s, 5, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
    let user = "show the softmax Jacobian as a matrix and the KL divergence between two Gaussians";
    feed(&mut s, 6, Event::TurnStarted { session: sid(), turn: 2, model: sonnet_ref(), user: user.into() });
    s
}

/// The conversation pane's text rows on screen.
fn pane_rows(buf: &ratatui::buffer::Buffer, pane: Rect) -> Vec<String> {
    (pane.y..pane.bottom())
        .map(|y| {
            let mut l = String::new();
            for x in pane.x..pane.right().saturating_sub(2) {
                l.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            l.trim_end().to_string()
        })
        .collect()
}

/// One frame of the replay.
struct Frame {
    tok: String,
    /// The transcript's first line on screen.
    start: usize,
    total: usize,
    rows: Vec<String>,
}

struct Replay {
    frames: Vec<Frame>,
    pane: Rect,
}

/// Feeds the reply `per_frame` tokens per frame (the run loop applies every queued delta before a
/// frame), with the view `scroll` lines up from the bottom once `scroll_from` tokens are in.
fn replay(w: u16, h: u16, per_frame: usize, scroll: usize, scroll_from: usize) -> Replay {
    let mut s = in_session();
    let pane = marlowe_tui::render::transcript_pane(marlowe_tui::render::geometry(Rect::new(0, 0, w, h), &s).conversation, &s);
    let toks = tokens(REPLY);
    let mut frames = Vec::new();
    let mut seq = 7;
    for (k, chunk) in toks.chunks(per_frame).enumerate() {
        for t in chunk {
            feed(&mut s, seq, Event::TextDelta { session: sid(), turn: 2, text: t.clone() });
            seq += 1;
        }
        if scroll > 0 && k * per_frame >= scroll_from && s.scroll == 0 {
            s.scroll = scroll;
        }
        frames.push(frame(&s, w, h, pane, chunk.concat()));
    }
    feed(&mut s, seq, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    frames.push(frame(&s, w, h, pane, "<turn finished>".into()));
    Replay { frames, pane }
}

fn frame(s: &State, w: u16, h: u16, pane: Rect, tok: String) -> Frame {
    let buf = render(s, w, h);
    let v = s.viewport.get();
    Frame { tok, start: v.top, total: v.total, rows: pane_rows(&buf, pane) }
}

/// Frames where the transcript got shorter while the reply streamed: (frame, rows lost). Each is
/// a frame the pane would have jumped back on, had it not held its place.
fn shrinks(r: &Replay) -> Vec<(usize, usize)> {
    r.frames
        .windows(2)
        .enumerate()
        .filter(|(_, p)| p[1].total < p[0].total)
        .map(|(k, p)| (k + 1, p[0].total - p[1].total))
        .collect()
}

/// Frames where the view went back up while following the tail: (frame, rows it jumped).
fn jumps_back(r: &Replay) -> Vec<(usize, usize)> {
    r.frames
        .windows(2)
        .enumerate()
        .filter(|(_, p)| p[1].start < p[0].start)
        .map(|(k, p)| (k + 1, p[0].start - p[1].start))
        .collect()
}

fn trace(r: &Replay, name: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "== {name}: pane {}x{}, {} frames", r.pane.width, r.pane.height, r.frames.len());
    let mut prev: Option<&Frame> = None;
    for (k, f) in r.frames.iter().enumerate() {
        let d = prev.map_or(0, |p| f.start as isize - p.start as isize);
        let _ = writeln!(out, "{k:4} start {:4} ({d:+3}) total {:4}  {:?}", f.start, f.total, f.tok);
        prev = Some(f);
    }
    out
}

fn dump_trace(r: &Replay, name: &str) {
    if let Ok(path) = std::env::var("MARLOWE_TRACE") {
        let text = trace(r, name);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
    }
}

/// Prints the frames around each jump back: the tokens that caused it and the bottom rows.
fn report(r: &Replay, name: &str) -> usize {
    let jumps = jumps_back(r);
    let up: usize = jumps.iter().map(|(_, n)| n).sum();
    println!(
        "== {name}: {} frames, view jumped back {} times, {} rows in all, largest {}",
        r.frames.len(),
        jumps.len(),
        up,
        jumps.iter().map(|(_, n)| *n).max().unwrap_or(0)
    );
    for (k, n) in jumps.iter().take(12) {
        let (p, f) = (&r.frames[k - 1], &r.frames[*k]);
        println!("  frame {k}: start {} -> {} (-{n}) after {:?}", p.start, f.start, f.tok);
        let tail = |fr: &Frame| fr.rows.iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join(" | ");
        println!("    before: {}", tail(p));
        println!("    after : {}", tail(f));
    }
    // `MARLOWE_SHOW=<frame>` prints that frame's pane and the one before it in full.
    if let Some(k) = std::env::var("MARLOWE_SHOW").ok().and_then(|v| v.parse::<usize>().ok()) {
        for f in &r.frames[k.saturating_sub(1)..=k.min(r.frames.len() - 1)] {
            println!("---- start {} after {:?}\n{}", f.start, f.tok, f.rows.join("\n"));
        }
    }
    dump_trace(r, name);
    jumps.len()
}

#[test]
fn the_replay_records_every_frame_and_the_reply_scrolls() {
    let r = replay(W, H, 1, 0, 0);
    assert!(r.frames.len() > 400, "the reply is long enough to replay token by token");
    assert!(r.frames.last().unwrap().start > 0, "the reply is taller than the pane");
    report(&r, "140x40, one token per frame");
    report(&replay(W, H, 3, 0, 0), "140x40, three tokens per frame");
    report(&replay(120, 30, 1, 0, 0), "120x30, one token per frame");
}

/// Following the stream, the view only ever moves on with the text, and the transcript under it
/// only ever grows (so the pane's hold on its place is never what keeps it still).
#[test]
fn following_the_stream_never_moves_the_view_back() {
    for (w, h, per) in [(W, H, 1), (W, H, 3), (120, 30, 1), (80, 24, 1), (W, H, 7)] {
        let r = replay(w, h, per, 0, 0);
        let n = report(&r, &format!("{w}x{h}, {per}/frame"));
        assert_eq!(n, 0, "{w}x{h}: the view jumped back {n} times while following the stream");
        let lost = shrinks(&r);
        assert!(lost.is_empty(), "{w}x{h}: the transcript got shorter at (frame, rows) {lost:?}");
        // And it kept up: the last streaming frame shows the end of the reply. The frame after it
        // is the turn finishing, when the strip's rows are freed under the text, which stays put.
        let n = r.frames.len();
        let (last, done) = (&r.frames[n - 2], &r.frames[n - 1]);
        assert_eq!(last.start + r.pane.height as usize, last.total, "{w}x{h}: the view fell behind the tail");
        assert_eq!(done.start, last.start, "{w}x{h}: the reply slid when the strip went away");
    }
}

/// A resize mid-stream lays everything out again at the new width, and the view follows the tail
/// at once rather than holding a line index from the old layout.
#[test]
fn a_resize_mid_stream_follows_the_tail_at_the_new_size() {
    let mut s = in_session();
    let toks = tokens(REPLY);
    let (half, rest) = toks.split_at(toks.len() / 2);
    let mut seq = 7;
    let mut push = |s: &mut State, ts: &[String]| {
        for t in ts {
            feed(s, seq, Event::TextDelta { session: sid(), turn: 2, text: t.clone() });
            seq += 1;
            render(s, W, H);
        }
    };
    push(&mut s, half);
    let wide = s.viewport.get();
    let _ = render(&s, 120, 30);
    let narrow = s.viewport.get();
    assert_eq!(narrow.top + narrow.size.1 as usize, narrow.total, "followed at the new size");
    assert_ne!(wide.size, narrow.size);
    push(&mut s, rest);
    let back = s.viewport.get();
    assert_eq!(back.top + back.size.1 as usize, back.total, "following the tail after resizing back");
}

/// A pane row without the `↓ N more` tag, whose count rightly grows as the reply does.
fn without_tag(row: &str) -> &str {
    row.split(" ↓ ").next().unwrap_or(row).trim_end()
}

/// Scrolled up to read, the rows on screen do not move while the reply streams on below.
#[test]
fn a_scrolled_up_view_stays_put_while_the_reply_streams() {
    let r = replay(W, H, 1, 12, 200);
    let from = r.frames.iter().position(|f| f.start + f.rows.len() + 12 <= f.total).expect("scrolled");
    let strip = |f: &Frame| f.rows.iter().map(|l| without_tag(l).to_string()).collect::<Vec<_>>();
    let pinned = strip(&r.frames[from]);
    // The last frame is the turn finishing: the strip's rows go and the tag moves down with the pane.
    let streamed = &r.frames[from..r.frames.len() - 1];
    let moved = streamed.iter().filter(|f| strip(f) != pinned).count();
    println!("== scrolled up 12 at token 200: {moved} of {} frames changed the rows on screen", r.frames.len() - from - 1);
    assert_eq!(moved, 0, "the reader's rows moved under them");
}

/// The rows each display formula in [`REPLY`] lays out to from every prefix a token stream
/// delivers, at the pane's formula width. This is why a held block does not reserve rows from
/// what has arrived: every fall was the tail shrinking under a reader following it, and the screen
/// jumping back by that many rows. A held block is one placeholder row instead.
fn reservation_falls(inner: usize) -> Vec<(String, Vec<usize>)> {
    let mut out = Vec::new();
    for (open, close) in [("$$\n", "\n$$"), ("\\[\n", "\n\\]")] {
        let mut rest = REPLY;
        while let Some(a) = rest.find(open) {
            let body = &rest[a + open.len()..];
            let Some(b) = body.find(close) else { break };
            let src = &body[..b];
            let mut so_far = String::new();
            let mut series = Vec::new();
            for t in tokens(src) {
                so_far.push_str(&t);
                series.push(marlowe_surface::math::reserve_rows(&so_far, inner));
            }
            let head: String = src.chars().take(40).collect();
            out.push((head, series));
            rest = &body[b + close.len()..];
        }
    }
    out
}

#[test]
fn the_held_rows_of_a_partial_formula_rise_and_fall_token_by_token() {
    // 140 columns: the pane is 96 wide, its text 92, a formula's 90.
    let pane = {
        let s = in_session();
        marlowe_tui::render::transcript_pane(marlowe_tui::render::geometry(Rect::new(0, 0, W, H), &s).conversation, &s)
    };
    let inner = pane.width as usize - 4 - 2;
    for (head, series) in reservation_falls(inner) {
        let falls: Vec<(usize, usize, usize)> = series
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[1] < w[0])
            .map(|(k, w)| (k + 1, w[0], w[1]))
            .collect();
        let finished = *series.last().unwrap();
        println!("{head:?}: rows {series:?}");
        println!("  {} falls (token, from, to): {falls:?}; landed at {finished}", falls.len());
    }
}

/// A token ending in a newline inside an unfinished display is not a blank line: the block stays
/// held rather than being drawn as finished for one frame.
#[test]
fn a_newline_inside_an_unfinished_display_keeps_it_held() {
    let th = theme();
    let base = ratatui::style::Style::default().fg(th.speech());
    let partial = concat!(r"\[", "\n", r"J = \begin{bmatrix}", "\n", r"a & b \\", "\n");
    let text = |src: &str| {
        marlowe_surface::markdown::render_prose_live(src, 90, &th, base)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let at_newline = text(partial);
    let next_token = text(&format!("{partial}c"));
    println!("after the newline:\n{at_newline}\nafter the next token:\n{next_token}");
    assert!(at_newline.contains('…'), "the unfinished matrix was drawn as finished:\n{at_newline}");
}

/// Reasoning collapsed with Ctrl+T while a reply streams: the pane follows the new, shorter tail
/// at once rather than hold a line index from the open layout over an almost empty pane.
#[test]
fn collapsing_reasoning_mid_stream_follows_the_new_tail() {
    let mut s = in_session();
    s.on_key(ctrl('t'));
    assert!(s.show_thinking, "reasoning opened");
    let step = "Weighing the Jacobian's diagonal against its off-diagonal terms before writing. ";
    let mut seq = 7;
    for _ in 0..300 {
        feed(&mut s, seq, Event::ReasoningDelta { session: sid(), turn: 2, text: step.into(), tokens: 12 });
        seq += 1;
        render(&s, W, H);
    }
    let open = s.viewport.get();
    assert!(open.total > 200, "a few hundred lines of reasoning: {}", open.total);
    s.on_key(ctrl('t'));
    assert!(!s.show_thinking);
    let pane = marlowe_tui::render::transcript_pane(marlowe_tui::render::geometry(Rect::new(0, 0, W, H), &s).conversation, &s);
    let check = |s: &State, when: &str| {
        let buf = render(s, W, H);
        let v = s.viewport.get();
        let rows = pane_rows(&buf, pane);
        assert!(v.total < open.total, "{when}: collapsed");
        assert_eq!(v.top + pane.height as usize, v.total.max(pane.height as usize), "{when}: not at the tail");
        assert!(rows.last().is_some_and(|r| !without_tag(r).is_empty()), "{when}: the last row is blank:\n{}", rows.join("\n"));
    };
    check(&s, "collapsed");
    // Reasoning goes on, collapsed: nothing is added to the transcript, and the view stays put.
    feed(&mut s, seq, Event::ReasoningDelta { session: sid(), turn: 2, text: step.into(), tokens: 12 });
    check(&s, "more reasoning");
}

/// Scrolled up while a reply streams, then the pane changes height (a multi-line paste grows the
/// composer; the window is made shorter) and width: the reader stays on the text they were
/// reading, not at the stale count of lines from the bottom they scrolled to.
#[test]
fn a_scrolled_up_view_keeps_its_place_through_a_paste_and_a_resize() {
    let mut s = in_session();
    let toks = tokens(REPLY);
    for (k, t) in toks.iter().enumerate().take(toks.len() * 3 / 4) {
        feed(&mut s, 7 + k as u64, Event::TextDelta { session: sid(), turn: 2, text: t.clone() });
        if k == 200 {
            s.scroll = 12;
        }
        render(&s, W, H);
    }
    let pane_of = |s: &State, w: u16, h: u16| marlowe_tui::render::transcript_pane(marlowe_tui::render::geometry(Rect::new(0, 0, w, h), s).conversation, s);
    // The top rows on screen, as the reader sees them.
    let top_rows = |s: &State, w: u16, h: u16| {
        let rows = pane_rows(&render(s, w, h), pane_of(s, w, h));
        rows.into_iter().take(6).map(|r| without_tag(&r).to_string()).collect::<Vec<_>>()
    };
    let reading = top_rows(&s, W, H);
    let before = s.viewport.get();
    assert!(before.top + (before.size.1 as usize) + 12 < before.total, "the reply grew below the reader");
    assert!(reading.iter().any(|r| !r.trim().is_empty()), "nothing to read: {reading:?}");

    let tall = pane_of(&s, W, H).height;
    s.on_paste("first pasted line\nsecond\nthird\nfourth");
    assert!(pane_of(&s, W, H).height < tall, "the paste grew the composer");
    assert_eq!(top_rows(&s, W, H), reading, "a paste moved the reader");

    assert_eq!(top_rows(&s, W, H - 6), reading, "a shorter window moved the reader");

    // A narrower window wraps every line again: the reader's text is still on screen.
    let buf = render(&s, 120, H);
    let rows = pane_rows(&buf, pane_of(&s, 120, H));
    let line = reading.iter().find(|r| r.split_whitespace().count() >= 3).expect("a line of words on screen");
    let words: String = line.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    assert!(
        rows.iter().any(|r| r.contains(&words)),
        "after a resize {words:?} is off screen:\n{}",
        rows.join("\n")
    );
}
