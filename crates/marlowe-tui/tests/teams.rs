//! The Teams toggle in the header: drawn off and on, lit under the pointer, clicked, keyed,
//! typed as `/teams on|off`, kept in step with the daemon, and still there at narrow widths.

mod common;

use common::*;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{Event, Request};
use marlowe_tui::state::Command;
use marlowe_tui::View;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE }
}

fn moved(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE }
}

fn area(w: u16) -> Rect {
    Rect::new(0, 0, w, H)
}

/// Where `needle` starts on the header row, or `None`.
fn on_header(buf: &Buffer, needle: &str) -> Option<u16> {
    let row = dump(buf).lines().next()?.to_string();
    row.find(needle).map(|i| row[..i].chars().count() as u16)
}

fn set_teams(on: bool) -> Request {
    Request::SetTeams { session: sid(), on }
}

#[test]
fn the_toggle_reads_off_then_on_in_the_accent() {
    let th = theme();
    let mut s = coding_session();
    let buf = render(&s, W, H);
    assert_palette_only(&buf);
    let x = on_header(&buf, "teams ○").expect("the toggle is in the header");
    assert_eq!(buf[(x, 0)].fg, th.dim_color(), "off is quiet");
    assert!(on_header(&buf, "teams ●").is_none());

    feed(&mut s, 3, Event::TeamsChanged { session: sid(), on: true, size: 10 });
    let buf = render(&s, W, H);
    assert_palette_only(&buf);
    let x = on_header(&buf, "teams ●").expect("on");
    assert_eq!(buf[(x, 0)].fg, th.accent(), "on is the accent");
    assert_eq!(buf[(x + 6, 0)].fg, th.accent());
    assert_eq!(s.code.team_size, 10);
}

#[test]
fn another_sessions_state_is_not_ours() {
    let mut s = coding_session();
    feed(&mut s, 3, Event::TeamsChanged { session: marlowe_proto::SessionId::from("other"), on: true, size: 10 });
    assert!(!s.code.teams);
}

#[test]
fn the_pointer_lights_the_toggle() {
    let th = theme();
    let mut s = coding_session();
    let buf = render(&s, W, H);
    let x = on_header(&buf, "teams ○").unwrap();
    s.on_mouse(moved(x + 2, 0), area(W));
    let buf = render(&s, W, H);
    assert_palette_only(&buf);
    assert_eq!(buf[(x, 0)].fg, th.normal().fg.unwrap_or(ratatui::style::Color::Reset), "off, hovered: body text");

    feed(&mut s, 3, Event::TeamsChanged { session: sid(), on: true, size: 10 });
    render(&s, W, H);
    s.on_mouse(moved(x + 2, 0), area(W));
    let buf = render(&s, W, H);
    assert_eq!(buf[(x, 0)].fg, th.speech(), "on, hovered: Marlowe's own colour");
    s.on_mouse(moved(0, 5), area(W));
    let buf = render(&s, W, H);
    assert_eq!(buf[(x, 0)].fg, th.accent(), "the pointer left");
}

#[test]
fn a_click_switches_teams_and_asks_the_daemon() {
    let mut s = coding_session();
    let buf = render(&s, W, H);
    let x = on_header(&buf, "teams ○").unwrap();
    // Any cell of the label is the target.
    for dx in [0, 3, 6] {
        let mut t = coding_session();
        render(&t, W, H);
        assert_eq!(t.on_mouse(click(x + dx, 0), area(W)), vec![set_teams(true)], "dx {dx}");
        assert!(t.code.teams, "shown at once");
        assert!(contains(&render(&t, W, H), "teams ●"));
    }
    assert_eq!(s.on_mouse(click(x - 1, 0), area(W)), vec![], "just left of it is not the toggle");
    assert!(!s.code.teams);
    assert_eq!(s.on_mouse(click(x, 0), area(W)), vec![set_teams(true)]);
    render(&s, W, H);
    assert_eq!(s.on_mouse(click(x, 0), area(W)), vec![set_teams(false)]);
    assert!(!s.code.teams);
}

#[test]
fn the_summary_still_opens_the_crew() {
    let mut s = coding_session();
    let buf = render(&s, W, H);
    let x = on_header(&buf, "0 runs").expect("the summary");
    s.on_mouse(click(x, 0), area(W));
    assert_eq!(s.view, View::Crew);
}

#[test]
fn the_key_and_the_slash_command_switch_it() {
    let mut s = coding_session();
    assert_eq!(s.on_key(alt('m')), vec![set_teams(true)]);
    assert_eq!(s.on_key(alt('m')), vec![set_teams(false)]);
    assert_eq!(keys(&mut s, "'/teams on' enter"), vec![set_teams(true)]);
    assert_eq!(keys(&mut s, "'/teams off' enter"), vec![set_teams(false)]);
    assert!(keys(&mut s, "'/teams maybe' enter").is_empty());
    assert!(!s.code.teams);
    // Bare `/teams` is still the view of team tasks.
    keys(&mut s, "'/teams' enter");
    assert!(s.code.panel.is_some());
    assert!(Command::ALL.contains(&Command::ToggleTeams));
}

#[test]
fn a_toggle_before_a_session_waits_for_one_and_an_adopted_session_asks_for_its_state() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    assert!(s.on_key(alt('m')).is_empty(), "no session to tell");
    assert!(s.code.teams);
    keys(&mut s, "'hello' enter");
    let out = feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "gate work".into(), workspace: None });
    assert!(out.contains(&set_teams(true)), "{out:?}");

    // A session opened without a pending choice asks for what the daemon remembers.
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    keys(&mut s, "'hello' enter");
    let out = feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "gate work".into(), workspace: None });
    assert!(out.contains(&Request::GetTeams { session: sid() }), "{out:?}");
    feed(&mut s, 3, Event::TeamsChanged { session: sid(), on: true, size: 6 });
    assert!(s.code.teams);
    assert_eq!(s.code.team_size, 6);
    // A new conversation starts without it until the daemon says.
    s.on_key(ctrl('n'));
    assert!(!s.code.teams);
}

#[test]
fn a_long_title_gives_way_before_the_toggle_and_the_summary() {
    for w in [140u16, 130, 120] {
        let mut s = busy_session();
        s.title = Some("a very long session title that goes on and on about gates and drift ".repeat(4));
        feed(&mut s, 3, Event::TeamsChanged { session: sid(), on: true, size: 10 });
        let buf = render(&s, w, H);
        assert_palette_only(&buf);
        let row = dump(&buf).lines().next().unwrap().to_string();
        assert!(row.chars().count() <= w as usize, "w {w}: {row:?}");
        let x = on_header(&buf, "teams ●").unwrap_or_else(|| panic!("w {w}: the toggle is kept: {row:?}"));
        let sx = on_header(&buf, "runs").unwrap_or_else(|| panic!("w {w}: the summary is kept: {row:?}"));
        assert!(x + 7 < sx, "w {w}: the toggle sits left of the summary, apart: {row:?}");
        assert!(row.contains('…'), "w {w}: the title is what was cut: {row:?}");
        assert_eq!(s.on_mouse(click(x + 1, 0), area(w)), vec![set_teams(false)], "w {w}");
    }
}

#[test]
fn below_the_minimum_window_there_is_no_header_to_toggle() {
    for w in [100u16, 60, 30, 8] {
        let s = busy_session();
        let buf = render(&s, w, H);
        assert_palette_only(&buf);
        assert!(!contains(&buf, "teams"), "w {w}");
    }
}
