//! **Reasoning folds: Enter or a click opens and closes one block, in place.**
//!
//! Matthew: "In the old TUI, pressing the Enter key opened the thinking block. Right now we have
//! no way to click to open the thinking block once thinking is done." Every finished block shows
//! `▸ thought for 12s · 600 tokens`; Enter on the Conversation box folds the block its cursor is
//! on (Shift+arrows move the cursor between blocks), and a click on a marker folds that one.
//!
//! These replay key and mouse events through the real pane at a real terminal size and record the
//! rows on screen. The rule they hold: **the toggled marker stays on the screen row it was on**,
//! for any block, any number of times, idle or while a later reply streams; and ^t still opens
//! or closes every block at once.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_model::{Part, Role, Thinking};
use marlowe_proto::{Envelope, Event, StoredTurn, TurnStop};
use marlowe_tui::render::geometry;
use marlowe_tui::state::{Focus, Pin, Target};
use marlowe_tui::{ClientEvent, Entry, State};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const T0: i64 = 1_800_000_000_000;

fn at(s: &mut State, seq: u64, at_ms: i64, event: Event) {
    s.on_client(ClientEvent::Envelope(Envelope { seq, at_ms, event }));
}

/// Eight lines of reasoning for turn `t`, each one row at this width.
fn reasoning(t: u32) -> String {
    (1..=8)
        .map(|k| format!("Turn {t}, step {k}: weigh the gate threshold against the eval drift first."))
        .collect::<Vec<_>>()
        .join("\n")
}

fn answer(t: u32) -> String {
    format!("Answer {t}. ") + &"The threshold moved because the eval harness pinned the tokenizer, so the drift is real. ".repeat(4)
}

/// A session of `turns` finished turns. Turn `t` thought for `10 + t` seconds, so its marker reads
/// `thought for 1Ns` and tells the turns apart on screen.
fn session(turns: u32) -> (State, u64) {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "folds".into(), workspace: None });
    s.session = Some(sid());
    let mut seq = 2;
    for t in 1..=turns {
        let t0 = T0 + t as i64 * 120_000;
        let mut ev = |s: &mut State, ms: i64, e: Event| {
            at(s, seq, t0 + ms, e);
            seq += 1;
        };
        ev(&mut s, 0, Event::TurnStarted { session: sid(), turn: t, model: sonnet_ref(), user: format!("question {t}") });
        ev(&mut s, 500, Event::ReasoningDelta { session: sid(), turn: t, text: reasoning(t), tokens: 600 });
        let took = 500 + (10 + t as i64) * 1000;
        ev(&mut s, took, Event::TextDelta { session: sid(), turn: t, text: answer(t) });
        ev(&mut s, took + 2_000, Event::TurnFinished { session: sid(), turn: t, usage: usage(), stop: TurnStop::Done });
    }
    (s, seq)
}

fn pane(s: &State) -> Rect {
    geometry(Rect::new(0, 0, W, H), s).conversation
}

/// The pane's text rows on screen, without the scrollbar or the `↓ N more` tag (whose count
/// rightly changes as a reply grows below).
fn pane_rows(buf: &Buffer, pane: Rect) -> Vec<String> {
    (pane.y..pane.bottom())
        .map(|y| {
            let mut l = String::new();
            for x in pane.x..pane.right().saturating_sub(2) {
                l.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            l.split(" ↓ ").next().unwrap_or("").trim_end().to_string()
        })
        .collect()
}

/// Draws a frame (which records where everything is), checks the palette, returns the pane rows.
fn rows(s: &State) -> Vec<String> {
    let buf = render(s, W, H);
    assert_palette_only(&buf);
    pane_rows(&buf, pane(s))
}

/// The pane row showing turn `t`'s marker.
fn row_of(rows: &[String], t: u32) -> Option<usize> {
    let needle = format!("thought for {}s", 10 + t);
    rows.iter().position(|r| r.contains(&needle))
}

fn is_open(rows: &[String], t: u32) -> bool {
    let r = row_of(rows, t).unwrap_or_else(|| panic!("turn {t}'s marker is not on screen:\n{}", rows.join("\n")));
    rows[r].trim_start().starts_with('▾')
}

/// The entry index of turn `t`'s reasoning block.
fn entry_of(s: &State, t: u32) -> usize {
    s.entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::Thinking { .. })).nth(t as usize - 1).unwrap().0
}

fn click_at(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn move_to(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: KeyModifiers::NONE }
}

/// Clicks turn `t`'s marker where the last frame drew it.
fn click_marker(s: &mut State, rows: &[String], t: u32) {
    let p = pane(s);
    let r = row_of(rows, t).expect("the marker is on screen");
    s.on_mouse(click_at(p.x + 3, p.y + r as u16), Rect::new(0, 0, W, H));
}

/// Draws a frame, then clicks turn `t`'s marker on it.
fn click_now(s: &mut State, t: u32) {
    let r = rows(s);
    click_marker(s, &r, t);
}

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

/// Answer tokens, a word at a time.
fn words(text: &str) -> Vec<String> {
    text.split_inclusive(' ').map(str::to_string).collect()
}

#[test]
fn a_finished_block_says_how_long_it_thought_and_a_stored_one_its_tokens() {
    let (s, _) = session(1);
    let r = rows(&s);
    assert!(r.iter().any(|l| l.trim() == "▸ thought for 11s · 600 tokens"), "{}", r.join("\n"));
    // Reopened from the store there is no timing: the count alone, and it still folds.
    let mut s = state();
    welcome(&mut s, 0);
    s.session = Some(sid());
    let turns = vec![
        StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "hello".into() }], at_ms: 0, phases: Vec::new(), numbers: None },
        StoredTurn {
            idx: 1,
            role: Role::Assistant,
            content: vec![
                Part::Thinking(Thinking { text: "Let me think about the gate.".into(), signature: None, details: None }),
                Part::Text { text: "Hello.".into() },
            ],
            at_ms: 0,
            phases: Vec::new(),
            numbers: None,
        },
    ];
    feed(&mut s, 0, Event::Transcript { session: sid(), turns });
    let r = rows(&s);
    let m = r.iter().position(|l| l.trim().starts_with("▸ thought · ")).expect("a stored block's marker");
    s.on_mouse(click_at(pane(&s).x + 3, pane(&s).y + m as u16), Rect::new(0, 0, W, H));
    let r = rows(&s);
    assert!(r[m].trim().starts_with("▾ thought · "));
    assert!(r[m + 1].contains("Let me think about the gate."), "{}", r.join("\n"));
}

/// Enter on the Conversation box opens the block its cursor is on, and again closes it, with the
/// marker on the same row throughout and the screen exactly as it was once closed.
#[test]
fn enter_folds_the_block_under_the_cursor_in_place_any_number_of_times() {
    let (mut s, _) = session(4);
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.focus, Focus::Conversation);
    let before = rows(&s);
    // Following the tail: the lowest marker on screen has the cursor, with Enter's keycap.
    let row = row_of(&before, 4).expect("the last turn's marker is on screen");
    assert!(before[row].ends_with('↵'), "the cursor's marker shows ↵: {:?}", before[row]);
    assert!(contains(&render(&s, W, H), "↵ open"), "the box's hints say what Enter does");
    for round in 0..3 {
        s.on_key(key(KeyCode::Enter));
        let open = rows(&s);
        assert_eq!(row_of(&open, 4), Some(row), "round {round}: opening moved the marker");
        assert!(is_open(&open, 4));
        assert!(open[row + 1].contains("Turn 4, step 1"), "the reasoning shows under its marker:\n{}", open.join("\n"));
        assert_eq!(open[..row], before[..row], "round {round}: rows above the marker moved");
        assert!(contains(&render(&s, W, H), "↵ close"));

        s.on_key(key(KeyCode::Enter));
        let closed = rows(&s);
        assert_eq!(closed, before, "round {round}: closing did not give back the screen it opened from");
    }
    // Only that block: the others follow ^t's default, closed.
    assert!(!s.thought_open(entry_of(&s, 3)));
}

/// Shift+↑ walks the cursor up to past turns, bringing an off-screen one into view; Enter folds
/// that block and no other, its marker staying put.
#[test]
fn shift_arrows_reach_past_blocks_and_enter_folds_only_that_one() {
    let (mut s, _) = session(4);
    s.on_key(key(KeyCode::Esc));
    let r0 = rows(&s);
    let top0 = s.viewport.get().top;
    assert!(row_of(&r0, 1).is_none(), "turn 1 starts off screen:\n{}", r0.join("\n"));
    for t in [3, 2] {
        s.on_key(shift(KeyCode::Up));
        let r = rows(&s);
        let at = row_of(&r, t).expect("on screen");
        assert!(r[at].ends_with('↵'), "the cursor moved to turn {t}: {:?}", r[at]);
        assert_eq!(r.iter().filter(|l| l.ends_with('↵')).count(), 1, "one cursor");
        assert_eq!(s.viewport.get().top, top0, "moving between markers on screen does not scroll");
    }
    // Turn 2 opens in place.
    let before = rows(&s);
    let at = row_of(&before, 2).unwrap();
    s.on_key(key(KeyCode::Enter));
    let after = rows(&s);
    assert_eq!(row_of(&after, 2), Some(at), "a past block's marker moved when it opened");
    assert!(is_open(&after, 2));
    assert!(!s.thought_open(entry_of(&s, 3)) && !s.thought_open(entry_of(&s, 4)) && !s.thought_open(entry_of(&s, 1)));
    // On up to turn 1, which was above the screen: the pane comes to it.
    s.on_key(shift(KeyCode::Up));
    let r = rows(&s);
    let at1 = row_of(&r, 1).expect("the pane brought turn 1 on screen");
    assert!(r[at1].ends_with('↵'));
    s.on_key(key(KeyCode::Enter));
    let r1 = rows(&s);
    assert_eq!(row_of(&r1, 1), Some(at1));
    assert!(is_open(&r1, 1) && is_open(&r1, 2));
    // And back down the way it came.
    s.on_key(shift(KeyCode::Down));
    let r = rows(&s);
    assert!(r[row_of(&r, 2).unwrap()].ends_with('↵'), "the cursor went back down to turn 2");
}

/// With no reasoning on screen, Enter on the Conversation box goes back to typing, as it did.
#[test]
fn enter_without_reasoning_on_screen_goes_back_to_the_message_box() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    s.session = Some(sid());
    feed(&mut s, 2, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "hi".into() });
    feed(&mut s, 3, Event::TextDelta { session: sid(), turn: 1, text: "Hello.".into() });
    feed(&mut s, 4, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
    s.on_key(key(KeyCode::Esc));
    rows(&s);
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.focus, Focus::Message);
}

/// A turn that only thought and never answered does not go silent: a dim line says so. A turn
/// that answered, or an Interrupted one (which has its own note), gets no such line.
#[test]
fn a_turn_that_ended_with_thinking_only_says_so() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    s.session = Some(sid());
    feed(&mut s, 2, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "where is the config?".into() });
    feed(&mut s, 3, Event::ReasoningDelta { session: sid(), turn: 1, text: "it is in conf.toml".into(), tokens: 5 });
    feed(&mut s, 4, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
    assert!(contains(&render(&s, 140, 40), "ended without a reply"));
    assert_palette_only(&render(&s, 140, 40));

    feed(&mut s, 5, Event::TurnStarted { session: sid(), turn: 2, model: sonnet_ref(), user: "and the log?".into() });
    feed(&mut s, 6, Event::TextDelta { session: sid(), turn: 2, text: "In run.log.".into() });
    feed(&mut s, 7, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    let notes = s.entries.iter().filter(|e| matches!(e, Entry::Note { text, .. } if text == "ended without a reply")).count();
    assert_eq!(notes, 1, "only the silent turn has the line");
}

/// The pointer lights a marker one weight up; a click folds that block where it is, again and
/// again, and leaves the keys in the Message box so Enter still sends.
#[test]
fn a_click_on_a_marker_folds_it_in_place_and_the_pointer_lights_it() {
    let th = theme();
    let (mut s, _) = session(4);
    let before = rows(&s);
    let p = pane(&s);
    let row = row_of(&before, 3).expect("turn 3's marker is on screen");
    let (x, y) = (p.x + 2, p.y + row as u16);
    let buf = render(&s, W, H);
    assert_eq!(buf[(x, y)].fg, th.dimmer_color(), "a finished marker rests in the dimmer weight");
    s.on_mouse(move_to(x + 4, y), Rect::new(0, 0, W, H));
    assert_eq!(s.hover, Some(Target::Thought(entry_of(&s, 3))));
    let buf = render(&s, W, H);
    assert_eq!(buf[(x, y)].fg, th.dim_color(), "under the pointer it goes one weight up");
    assert_eq!(buf[(x + 4, y)].bg, ratatui::style::Color::Reset, "never a fill");
    // Beside the marker is not the marker.
    s.on_mouse(move_to(p.x + 60, y), Rect::new(0, 0, W, H));
    assert_ne!(s.hover, Some(Target::Thought(entry_of(&s, 3))));

    for round in 0..4 {
        click_now(&mut s, 3);
        let open = rows(&s);
        assert_eq!(row_of(&open, 3), Some(row), "round {round}: the clicked marker moved");
        assert!(is_open(&open, 3), "round {round}");
        assert!(open[row + 1].contains("Turn 3, step 1"));
        assert_eq!(open[..row], before[..row]);
        click_marker(&mut s, &open, 3);
        assert_eq!(rows(&s), before, "round {round}: closing did not give back the screen");
    }
    // A past block, then a later one: each opens on its own.
    click_marker(&mut s, &before, 2);
    let r = rows(&s);
    assert!(is_open(&r, 2));
    assert_eq!(row_of(&r, 2), row_of(&before, 2));
    click_marker(&mut s, &r, 3);
    let r = rows(&s);
    assert!(is_open(&r, 2) && is_open(&r, 3));
    assert_eq!(s.focus, Focus::Message, "a click does not take the keys");
    type_str(&mut s, "next");
    let sent = s.on_key(key(KeyCode::Enter));
    assert!(matches!(&sent[..], [marlowe_proto::Request::Ask { .. }]), "Enter still sends: {sent:?}");
}

/// ^t still opens and closes every block at once, over whatever was folded one at a time.
#[test]
fn ctrl_t_still_opens_and_closes_every_block() {
    let (mut s, _) = session(3);
    click_now(&mut s, 2);
    assert!(is_open(&rows(&s), 2));
    s.on_key(ctrl('t'));
    assert!(s.show_thinking);
    s.scroll = s.scroll_max.get();
    let r = rows(&s);
    for t in 1..=3 {
        if row_of(&r, t).is_some() {
            assert!(is_open(&r, t), "^t opened turn {t}");
        }
    }
    s.on_key(ctrl('t'));
    let r = rows(&s);
    for t in 1..=3 {
        if row_of(&r, t).is_some() {
            assert!(!is_open(&r, t), "^t closed turn {t}, the one opened by hand too");
        }
    }
    assert!(s.folds.is_empty());
}

/// While a later reply streams, a click opens an earlier block where it is, and the reader's rows
/// then stay put however much arrives below, until they go back to the latest.
#[test]
fn a_block_opened_while_a_reply_streams_holds_the_readers_rows() {
    let (mut s, mut seq) = session(4);
    let t0 = T0 + 5 * 120_000;
    at(&mut s, seq, t0, Event::TurnStarted { session: sid(), turn: 5, model: sonnet_ref(), user: "question 5".into() });
    seq += 1;
    at(&mut s, seq, t0 + 500, Event::ReasoningDelta { session: sid(), turn: 5, text: reasoning(5), tokens: 600 });
    seq += 1;
    let reply = answer(5).repeat(6);
    let toks = words(&reply);
    let mut stream = |s: &mut State, n: usize, k: &mut usize, frames: &mut Vec<(usize, Vec<String>)>| {
        for t in toks.iter().skip(*k).take(n) {
            // Turn 5 thinks for 30s: its marker is not mistaken for turn 4's 14s.
            at(s, seq, t0 + 30_500, Event::TextDelta { session: sid(), turn: 5, text: t.clone() });
            seq += 1;
            let r = rows(s);
            frames.push((s.viewport.get().top, r));
        }
        *k += n;
    };
    let mut k = 0;
    let mut frames = Vec::new();
    stream(&mut s, 20, &mut k, &mut frames);
    let before = frames.last().unwrap().1.clone();
    let row = row_of(&before, 4).expect("turn 4's marker is on screen while the reply streams");

    click_marker(&mut s, &before, 4);
    let open = rows(&s);
    assert_eq!(row_of(&open, 4), Some(row), "opening mid-stream moved the marker");
    assert!(is_open(&open, 4));
    assert_eq!(s.viewport.get().pin, Pin::Above, "the reader is now above the tail, reading");

    let top = s.viewport.get().top;
    let mut held = Vec::new();
    stream(&mut s, 60, &mut k, &mut held);
    // The last row carries the `↓ N more` tag, which widens as N grows and covers more of it.
    let body = |r: &[String]| r[..r.len() - 1].to_vec();
    let moved = held.iter().filter(|(t, r)| *t != top || body(r) != body(&open)).count();
    assert_eq!(moved, 0, "the reader's rows moved while the reply streamed below them");

    // Closed again mid-stream: the marker stays, and the view never goes back up after.
    click_marker(&mut s, &open, 4);
    let closed = rows(&s);
    assert_eq!(row_of(&closed, 4), Some(row), "closing mid-stream moved the marker");
    assert!(!is_open(&closed, 4));
    let mut after = Vec::new();
    stream(&mut s, 60, &mut k, &mut after);
    let mut last = (s.viewport.get().top, row);
    for (top, r) in &after {
        assert!(*top >= last.0, "the view went back up");
        if let Some(at) = row_of(r, 4) {
            assert!(at <= last.1, "turn 4's marker moved down");
            last.1 = at;
        }
        last.0 = *top;
    }

    // Back to the latest: the view follows the reply again.
    s.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    stream(&mut s, 5, &mut k, &mut Vec::new());
    let v = s.viewport.get();
    assert_eq!(v.top + v.size.1 as usize, v.total, "^End follows the tail");
}

/// Closing a block near the end while a reply streams leaves the rows under it empty rather than
/// pull every line down; the reply fills them, and then the view follows the tail. The marker
/// never moves down and the view never goes back up.
#[test]
fn closing_a_block_near_the_end_mid_stream_never_moves_the_view_back() {
    let (mut s, mut seq) = session(4);
    // Turn 4's block opened at the tail, then a new message sent.
    click_now(&mut s, 4);
    assert!(is_open(&rows(&s), 4));
    type_str(&mut s, "question 5");
    s.on_key(key(KeyCode::Enter));
    let t0 = T0 + 5 * 120_000;
    at(&mut s, seq, t0, Event::TurnStarted { session: sid(), turn: 5, model: sonnet_ref(), user: "question 5".into() });
    seq += 1;
    let toks = words(&answer(5).repeat(5));
    let mut k = 0;
    for t in toks.iter().take(4) {
        at(&mut s, seq, t0 + 3_000, Event::TextDelta { session: sid(), turn: 5, text: t.clone() });
        seq += 1;
        rows(&s);
        k += 1;
    }
    let v = s.viewport.get();
    assert_eq!(v.top + v.size.1 as usize, v.total, "following the new reply");
    let before = rows(&s);
    let row = row_of(&before, 4).expect("turn 4's open block is on screen");
    assert!(is_open(&before, 4));

    click_marker(&mut s, &before, 4);
    let closed = rows(&s);
    assert_eq!(row_of(&closed, 4), Some(row), "closing moved the marker");
    assert_eq!(s.viewport.get().pin, Pin::Tail);
    assert!(closed.last().is_some_and(|r| r.trim().is_empty()), "the rows under it wait empty:\n{}", closed.join("\n"));

    let mut prev = (s.viewport.get().top, row);
    for t in toks.iter().skip(k) {
        at(&mut s, seq, t0 + 4_000, Event::TextDelta { session: sid(), turn: 5, text: t.clone() });
        seq += 1;
        let r = rows(&s);
        let top = s.viewport.get().top;
        assert!(top >= prev.0, "the view went back up");
        if let Some(at) = row_of(&r, 4) {
            assert!(at <= prev.1, "turn 4's marker moved down");
            prev.1 = at;
        }
        prev.0 = top;
    }
    let v = s.viewport.get();
    assert_eq!(v.pin, Pin::None);
    assert_eq!(v.top + v.size.1 as usize, v.total, "the reply filled the rows and the view follows it");
}

/// Past the tail after a close, the wheel scrolls from where the view is, a few rows at a time.
#[test]
fn the_wheel_moves_from_a_view_left_past_the_tail() {
    let (mut s, _) = session(4);
    // Opened, then ^End in the same batch of input (no frame between): ^End wins.
    click_now(&mut s, 4);
    s.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let open = rows(&s);
    assert!(is_open(&open, 4));
    let v = s.viewport.get();
    assert_eq!(v.top + v.size.1 as usize, v.total, "^End after a click goes to the latest");
    let row = row_of(&open, 4).unwrap();
    click_marker(&mut s, &open, 4);
    let closed = rows(&s);
    assert_eq!(row_of(&closed, 4), Some(row));
    assert_eq!(s.viewport.get().pin, Pin::Tail);
    let up = MouseEvent { kind: MouseEventKind::ScrollUp, column: pane(&s).x + 4, row: pane(&s).y + 4, modifiers: KeyModifiers::NONE };
    s.on_mouse(up, Rect::new(0, 0, W, H));
    let r = rows(&s);
    assert_eq!(row_of(&r, 4), Some(row + 3), "one notch is three rows, not a jump to the tail");
}
