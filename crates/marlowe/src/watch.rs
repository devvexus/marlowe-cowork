//! **`marlowe --watch <run>` — a real terminal window on one run.** `M3-DESIGN.md` §6.
//!
//! # Why this is a second process and not a tab
//!
//! §6's first sentence: *"Multiple real TUI windows, not tabs."* A tab would put a run's output in
//! the same frame as the conversation, which is what §6.6's first interface rule exists to stop:
//! *"Filling the main pane with agent output halts the conversation visually, which is what this
//! milestone exists to stop."*
//!
//! A second process also makes §10.1's *"addressable from outside"* literal rather than aspirational.
//! This is an ordinary command. It works in a terminal Marlowe did not open, over SSH, from a
//! script — and that is exactly why [`crate::launcher::open_window`] is allowed to be best-effort:
//! the fallback is a command line, and a command line always works.
//!
//! # What this file does NOT own
//!
//! **The connection.** Session A's [`marlowe_daemon::Client`] finds the control plane, authenticates
//! and falls back to the main port when no control listener is running. A second client here would
//! be two answers to "where is the daemon", and the one that survives is the one that already
//! handles the fall-back.
//!
//! **The projection.** [`RunProjection`] folds `RunDetail` and `RunOutput` into the view.
//!
//! # The loop
//!
//! Poll, render, read a key, repeat. [`POLL_MS`] is the pacing and it is deliberately short: this is
//! a window on a live run, and §B5's rule is that motion means Marlowe is working.
//!
//! **There is no clock anywhere in the window path — not in the surface and not in this file.**
//! Elapsed is resolved on the daemon (see `ControlPlane::detail`), so a frame is a pure function of
//! the state this loop last fetched; and the pacing is `event::poll`'s own bounded wait, so nothing
//! here measures time either. `determinism_guard.rs` is what made that true rather than nearly true.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use marlowe_daemon::watch_client::RunProjection;
use marlowe_daemon::{Client, Event};
use marlowe_surface::app::Key;
use marlowe_surface::window::{self, Action, WindowApp, WindowRequest};
use marlowe_surface::Theme;

/// How often the window re-asks the control plane what is true.
///
/// **A poll, not a subscription.** 120 ms is under the threshold at which streamed prose reads as
/// arriving rather than appearing, and it is a loopback round trip against an in-memory map. The
/// cost is one connection per poll on a socket doing nothing else.
pub const POLL_MS: u64 = 120;

/// How many idle polls a notice survives — 25, which is **about three seconds** at [`POLL_MS`].
///
/// # Why this counts polls instead of reading a clock
///
/// *"Steer sent — it applies at the run's next step"* sat in the steer field until the next
/// keystroke, and the field shows a notice **instead of** the draft. A person who read it and did
/// not immediately type saw a composer with a sentence in it and no cursor, which reads as *"you
/// cannot type here any more"*. The steer had gone through; the field looked broken while working.
///
/// The obvious fix is an expiry timestamp, and this window path deliberately has no clock in it:
/// §6.4 makes a frame a pure function of state, `window_flicker.rs` asserts two windows on one
/// run's state are the same frame, and `determinism_guard.rs` greps this workspace for
/// `Instant::now`. A `now_ms` compared during a draw would break all three.
///
/// **So the driver ages it, and the surface never learns time exists.** `event::poll` returning
/// false is the tick — and it is the right tick, because a timeout means the user is not typing,
/// which is exactly the situation the notice looks broken in. The count is state the loop holds,
/// nothing is measured, and `WindowApp` renders what it is handed as before.
///
/// The honest consequence: this is *about* three seconds, not three seconds. A slow poll makes it
/// longer. For a transient acknowledgement that is the right trade.
pub const NOTICE_POLLS: u32 = 25;

/// Age a transient notice by one idle tick, clearing it when it has been up long enough.
///
/// Pulled out of the loop so it can be tested without a terminal — the loop itself needs a
/// tty, raw mode and a daemon, which is why the bug it fixes survived a suite with twelve
/// window tests in it.
pub(crate) fn age_notice(notice: &mut Option<String>, age: &mut u32) {
    if notice.is_none() {
        *age = 0;
        return;
    }
    *age += 1;
    if *age >= NOTICE_POLLS {
        *notice = None;
        *age = 0;
    }
}


pub struct Options {
    pub run: String,
    pub profile_root: std::path::PathBuf,
    pub port: Option<u16>,
    pub color_depth: Option<String>,
}

/// Open a window on a run. Returns when the user detaches.
///
/// **Detaching, never cancelling** (§6.5). There is no path out of this function that stops a run
/// except the one the user confirmed at the overlay.
pub fn run(opts: Options) -> io::Result<()> {
    let theme = match Theme::resolve(|k| std::env::var(k).ok(), opts.color_depth.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // **Everything that can refuse, refuses before the alternate screen.** A message printed inside
    // a raw-mode alternate buffer that is then torn down is a message nobody reads — the same
    // reason `tui.rs` checks the terminal size before `enable_raw_mode`.
    let mut client = Client::new("watch").with_profile_root(&opts.profile_root);
    if let Some(port) = opts.port {
        client = client.with_port(port);
    }

    let mut projection = RunProjection::new(&opts.run);
    match client.watch(&opts.run, 0) {
        Ok(events) => projection.apply(&events),
        Err(e) => {
            eprintln!("marlowe: {e}");
            std::process::exit(1);
        }
    }
    let Some(view) = projection.view() else {
        eprintln!(
            "marlowe: this daemon holds no run {}. `marlowe --runs` lists them",
            opts.run
        );
        std::process::exit(1);
    };

    let (cols, rows) = terminal::size()?;
    if cols < window::MIN_COLS || rows < window::MIN_ROWS {
        eprintln!(
            "a run window needs {}x{}; this terminal is {cols}x{rows}.\nResize it, or run \
             `marlowe --runs {}` for the same facts without the grid.",
            window::MIN_COLS,
            window::MIN_ROWS,
            opts.run
        );
        std::process::exit(1);
    }

    let mut app = WindowApp::new(view);

    // The panic hook first, for `tui.rs`'s reason: a panic inside raw mode leaves a terminal that
    // echoes nothing and is still on the alternate buffer, with the message invisible.
    install_panic_hook();
    terminal::enable_raw_mode()?;
    // **§B10, amended 2026-08-08: the mouse is captured.** The window shipped without this, so the
    // terminal kept its own drag-selection and the frame could be swiped like a printout — which
    // is the exact thing that amendment overruled the earlier reasoning to prevent: *"being able
    // to drag-select the frame is the single thing that makes a running application read as a
    // printout."* The conversation pane has captured since M1; this was a second answer to a
    // settled question, and the visible half of it was the run window feeling like output.
    //
    // The escape hatch is unchanged and is the one §B10 documents: **`Shift`-drag falls through**
    // to native selection in Windows Terminal, iTerm2 and GNOME Terminal.
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        // **`tui.rs` has hidden the cursor since M1 and this file never did.** So the terminal's
        // own caret sat blinking wherever the last write left it, on a surface whose focus is
        // signalled by §B2's border swaps — a second cursor the design does not have. It was the
        // other half of the block glyph the steer field used to draw: two caret-ish things, neither
        // of them the product's.
        crossterm::cursor::Hide,
        // **The terminal tells us it was a paste.** Without this a 340-line block arrives as 340
        // key events and the composer has to guess from typing speed — a guess that fires on a
        // fast typist and is invisible to every test.
        EnableBracketedPaste,
    )?;
    // The terminal's own title bar, which no sanitiser in this workspace covers — so every
    // character of it is the harness's own. See `window::title`.
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]0;{}\x07", window::title(app.view()));
    let _ = out.flush();

    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut term, &mut app, &client, &opts.run, &theme, projection);

    terminal::disable_raw_mode()?;
    execute!(
        io::stdout(),
        crossterm::cursor::Show,
        terminal::LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
    )?;
    result
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut WindowApp,
    client: &Client,
    run_id: &str,
    theme: &Theme,
    mut projection: RunProjection,
) -> io::Result<()> {
    // Idle polls since the current notice went up. See `NOTICE_POLLS`.
    let mut notice_age = 0u32;
    // **`scroll_max` costs a full markdown-and-wrap pass over the run's output**, and `draw` does
    // that pass again — so an idle window with a long transcript was rendering its transcript
    // TWICE every 120 ms to answer a question whose answer had not changed.
    //
    // The extent moves only when the output does, when the viewport does, or when a key has
    // altered the steer field's height. Nothing else can change it, and a scroll key cannot: it
    // moves the offset, not the number of lines.
    //
    // **This is a driver-side cache, not state written during a draw.** `WindowApp` still holds no
    // value produced by rendering, so §6.4's purity and `window_flicker.rs` are untouched — the
    // reason `scroll_max_hint` is set from outside in the first place.
    let mut extent_stale = true;
    let mut last_area: Option<ratatui::layout::Rect> = None;
    loop {
        // LOOP-EXEMPT: a surface's event loop, not an agent loop.
        //
        // **The key wait IS the pacing, and that is why this process reads no clock.**
        //
        // It was an `Instant` and an elapsed check. `determinism_guard.rs` flagged it — correctly:
        // §4.5 fences real time to `clock.rs`, and "it is only a poll interval" is the argument
        // every stray read comes with. `event::poll` already blocks for a bounded time and already
        // tells us which happened, so the timeout is a cadence and the return value is the reason.
        // Nothing measures anything, and there is nothing to fence.
        //
        // A key arriving instead of a timeout does not skip the refresh: the handler below polls
        // again once it has sent whatever the key asked for, so a window under continuous typing
        // still tracks the run.
        // **DRAW FIRST, THEN WAIT.** The order is the whole of this window's input latency, and
        // it was the other way round until someone typed into it in a real terminal.
        //
        // The draw used to sit *after* the poll. A keystroke was therefore handled at the bottom
        // of the iteration, and the frame containing that character was not drawn until the top
        // of the next one — behind `event::poll`, which blocks for up to POLL_MS. Type one letter
        // and pause, and it appeared **120 ms later**. Under continuous typing the queued keys
        // made `poll` return at once and it felt fine, which is exactly why no amount of using it
        // quickly would show the fault: it is worst for a single considered keystroke.
        //
        // `tui.rs` has always drawn at the bottom of its loop, immediately after handling input,
        // and the difference is the one the human described as "the main one feels ultra fast and
        // this one feels like 25 Hz". Same rule, stated once: **nothing may block between
        // handling an event and painting its result.**
        let area = term.size().map(|s| ratatui::layout::Rect::new(0, 0, s.width, s.height))?;
        if extent_stale || last_area != Some(area) {
            app.set_scroll_max(window::scroll_max(app, theme, area));
            last_area = Some(area);
            extent_stale = false;
        }
        term.draw(|f| window::draw(app, theme, f.area(), f.buffer_mut()))?;

        if !event::poll(Duration::from_millis(POLL_MS))? {
            extent_stale |= poll_run(client, run_id, app, &mut projection);
            // **Only on the idle path.** A notice must not expire out from under someone who is
            // typing — and a timeout is precisely the case where it looks like a dead field.
            age_notice(&mut app.notice, &mut notice_age);
            continue;
        }
        // **Capturing the mouse means owning it.** §B10: *"the mouse adds no capability the
        // keyboard lacks and no second state path"* — so a click and the wheel both route into the
        // same dispatch the arrow keys use rather than growing paths of their own. Without this,
        // capture would have *taken away* the terminal's native wheel and given nothing back.
        let key = match event::read()? {
            // **A burst of queued key events is a paste**, on the platform where the terminal will
            // not say so. See `keyburst` — `Event::Paste` below still handles the platforms where
            // it does arrive, and this handles the one this project develops on.
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => {
                match crate::keyburst::read_burst(k)? {
                    crate::keyburst::Burst::Paste(text) => {
                        app.paste(text);
                        // A paste grows the steer field, which shortens the output panel.
                        extent_stale = true;
                        continue;
                    }
                    crate::keyburst::Burst::Keys(keys) => {
                        // Every key in order. A burst that was not a paste must lose nothing.
                        let mut close = false;
                        for k in keys {
                            let Some(key) = translate(k.code, k.modifiers) else { continue };
                            if handle_key(app, client, run_id, &mut projection, key) {
                                close = true;
                            }
                        }
                        notice_age = 0;
                        extent_stale = true;
                        if close {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
            // One event for the whole block, straight into the composer.
            TermEvent::Paste(text) => {
                app.paste(text);
                continue;
            }
            TermEvent::Mouse(m) => {
                // The same derivation the draw uses — a grown steer field moves every border
                // above it, and hit-testing against the resting layout would put the click
                // targets where the borders used to be.
                let chrome = window::chrome_for(app, area);
                let hit = window::region_at(&chrome, m.column, m.row);
                match m.kind {
                    // **A click focuses, exactly as `Tab` does.** The conversation pane has done
                    // this since M1 — clicking the message field is how anyone starts typing in it
                    // — and a window where the only way to reach the steer field was `Tab` was a
                    // surface that looked clickable and was not. `continue` redraws at the top of
                    // the loop, so the focus ring moves under the click rather than one poll later.
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(id) = hit {
                            app.focus = id;
                        }
                        continue;
                    }
                    // Focus follows the wheel, then the ordinary arrow-key path does the work —
                    // `tui.rs`'s rule and its reason: scrolling a region you are not focused on
                    // puts the scrollbar and the focus ring in disagreement.
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        if let Some(id) = hit {
                            app.focus = id;
                        }
                        if m.kind == MouseEventKind::ScrollUp {
                            Key::Up
                        } else {
                            Key::Down
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        };

        notice_age = 0;
        extent_stale = true;
        if handle_key(app, client, run_id, &mut projection, key) {
            return Ok(());
        }
    }
}

/// One key, all the way through: dispatch, serve whatever it asked for, confirm a write.
/// Returns `true` when the window should close.
///
/// **Extracted so a burst can replay keys through exactly the same path.** A second dispatch for
/// the replayed case is the two-sides-silently-disagree shape applied to the keyboard.
fn handle_key(
    app: &mut WindowApp,
    client: &Client,
    run_id: &str,
    projection: &mut RunProjection,
    key: Key,
) -> bool {
    let action = app.on_key(key);
    // §B10's copy. The payload was built by the surface; writing it needs stdout, which is the
    // driver's job. OSC 52 goes straight out rather than through ratatui's buffer — it is not a
    // cell, and queueing it would tie a clipboard write to a repaint.
    if let Some(text) = app.pending_copy.take() {
        let mut out = io::stdout();
        let _ = write!(out, "{}", marlowe_surface::clipboard::osc52(&text));
        let _ = out.flush();
    }
    // **A round trip per WRITE, not per KEYSTROKE.** This ran unconditionally, so every ordinary
    // character typed into the steer field paid a TCP connect, a request and a response to the
    // control plane before the next frame could be drawn. Confirming a write is a real reason —
    // `pending_steers` moving is what says a steer landed — but it only applies when a write
    // happened. A letter going into a draft sends nothing.
    let mut wrote = false;
    for request in app.drain_requests() {
        wrote = true;
        match apply(client, run_id, request) {
            Ok(Some(note)) => app.notice = Some(note),
            Ok(None) => {}
            // **The control plane's own words**, verbatim: a refusal reworded here would lose
            // whatever the daemon said about why.
            Err(e) => app.refused(e),
        }
    }
    if wrote {
        let _ = poll_run(client, run_id, app, projection);
    }
    action == Action::Close
}

/// Ask the control plane what is true, and re-project it.
/// Returns whether the run's state actually moved, which is what decides if the scroll extent
/// needs recomputing — see the call site.
fn poll_run(
    client: &Client,
    run_id: &str,
    app: &mut WindowApp,
    projection: &mut RunProjection,
) -> bool {
    match client.watch(run_id, projection.since()) {
        Ok(events) => {
            let moved = !events.is_empty();
            // **A degraded frame is shown, not swallowed.** The plane sends one when a window has
            // fallen off the end of the frame ring; a gap the reader cannot see is worse than a
            // shorter history.
            for e in &events {
                if let Event::Degraded { what, remedy } = e {
                    app.notice = Some(format!("{what} — {remedy}"));
                }
            }
            projection.apply(&events);
            if let Some(v) = projection.view() {
                app.update(v);
            }
            moved
        }
        // **The daemon going away does not close the window**, it says so. A window that vanished
        // when a daemon restarted would take the user's steer draft with it.
        Err(e) => {
            app.notice = Some(format!("the control plane is unreachable: {e}"));
            true
        }
    }
}

/// Send one request the window made. `Ok(Some(_))` is a note worth showing.
fn apply(
    client: &Client,
    run_id: &str,
    request: WindowRequest,
) -> Result<Option<String>, marlowe_daemon::ClientError> {
    match request {
        // **The same request `/steer` and `marlowe --steer` send** (ADR-054). The text crosses
        // unvalidated; the daemon's admission is the one door, and a cap here would be a second cap.
        WindowRequest::Steer(text) => {
            client.steer(run_id, &text)?;
            Ok(Some("steer sent — it applies at the run's next step".into()))
        }
        WindowRequest::Cancel => {
            client.cancel(run_id)?;
            Ok(Some("cancelling at the next step".into()))
        }
        WindowRequest::Resume => {
            client.resume(run_id)?;
            Ok(Some("resume requested".into()))
        }
        // Nothing crosses the wire. §6.5: closing a window is a fact about the window.
        WindowRequest::Detach => Ok(None),
    }
}

fn translate(code: KeyCode, mods: KeyModifiers) -> Option<Key> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let shift = mods.contains(KeyModifiers::SHIFT);
    Some(match code {
        // **Three chords, one meaning** — see `Key::CtrlBackspace`. Collapsed here so no handler
        // has to know which terminal the user is on.
        KeyCode::Backspace if ctrl => Key::CtrlBackspace,
        KeyCode::Char('w') if ctrl => Key::CtrlBackspace,
        KeyCode::Char('h') if ctrl => Key::CtrlBackspace,
        KeyCode::Char(c) if ctrl => Key::Ctrl(c),
        // ADR-056's second namespace. After the Ctrl arms, so a Ctrl-Alt chord stays Ctrl's.
        KeyCode::Char(c) if alt => Key::Alt(c),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter if shift => Key::ShiftEnter,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    })
}

fn install_panic_hook() {
    // **DisableMouseCapture belongs here as much as LeaveAlternateScreen does.** A panic that
    // restored the screen and not the mouse leaves a shell printing escape sequences on every
    // click, which reads as a broken terminal rather than as a crashed program.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
        );
        previous(info);
    }));
}

// **There is no test here asserting the surface reads no clock, and that is deliberate.**
//
// There was one: a grep over `marlowe-surface/src/window.rs` for `SystemTime`, `Instant::now` and
// `now_ms`. It was a **declaration-site** check of a property that already has an
// **enforcement-site** one — `window_flicker.rs::two_windows_on_one_runs_state_are_the_same_frame`
// renders two real buffers from one run's state and compares them cell by cell. A frame that
// depended on the clock would differ there; a grep only says the words are absent.
//
// It also broke on its own prose the moment this file explained why the clock had gone, which is
// the tell that it was matching text rather than behaviour. Family #16 inverted: not a control
// nothing reads, but a control reading the wrong thing.
//
// `marlowe/tests/determinism_guard.rs` is the workspace-wide backstop for a real clock appearing
// anywhere outside the fences, and it covers this crate too.

#[cfg(test)]
mod tests {
    use super::*;

    /// **The defect, as a test.** A notice that never expires makes the steer field read as dead:
    /// it is shown *instead of* the draft, so after "steer sent" a person who did not immediately
    /// type saw a sentence where their cursor should be.
    #[test]
    fn a_notice_disappears_on_its_own_after_about_three_seconds_of_not_typing() {
        let mut notice = Some("steer sent — it applies at the run's next step".to_string());
        let mut age = 0;
        for _ in 0..NOTICE_POLLS - 1 {
            age_notice(&mut notice, &mut age);
        }
        assert!(notice.is_some(), "it expired early — the user would lose the acknowledgement");
        age_notice(&mut notice, &mut age);
        assert!(notice.is_none(), "the notice never expires; the field still reads as dead");
    }

    /// The control. Without this the test above would pass on an implementation that cleared the
    /// notice on the very first tick, which is a different bug wearing the same green.
    #[test]
    fn a_notice_survives_a_single_tick() {
        let mut notice = Some("copied".to_string());
        let mut age = 0;
        age_notice(&mut notice, &mut age);
        assert!(notice.is_some());
    }

    #[test]
    fn an_absent_notice_keeps_the_counter_at_zero() {
        // Otherwise a long quiet spell would age a counter that has nothing to age, and the NEXT
        // notice would flash for a fraction of its life.
        let mut notice = None;
        let mut age = 7;
        age_notice(&mut notice, &mut age);
        assert_eq!(age, 0);
    }
}
