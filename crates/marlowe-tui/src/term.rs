//! The terminal: raw mode, alternate screen, mouse, the input thread and the main loop.
//!
//! On the way in the terminal's default background becomes `#0f0e14` and the window title
//! `marlowe — <session>` (see [`crate::osc`]); every way out — quitting, an error, a panic, the
//! console closing — puts both back.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{never, select, unbounded, Receiver, Sender};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor, execute, terminal};
use marlowe_surface::Theme;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::client::Client;
use crate::coding::{Effect, LocalEvent};
use crate::images::Emitter;
use crate::links::LinkEmitter;
use crate::local;
use crate::osc;
use crate::pacing::Pacer;
use crate::render;
use crate::state::{ClientEvent, State};
use crate::TuiError;

/// Set while an editor owns the terminal: the input thread leaves the console alone.
static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);

/// What the console's close handler does first (track N): tell the daemon the window closed, so
/// it pauses its work, frees the GPU and saves before it exits. The handler has a few seconds
/// and no event loop, so this writes straight to the connection.
static CLOSE_NOTICE: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

fn notify_close() {
    if let Some(f) = CLOSE_NOTICE.get() {
        f();
    }
}

/// What the input thread delivers.
enum Input {
    Keys(Vec<KeyEvent>),
    Paste(String),
    Mouse(MouseEvent),
    Resize,
}

/// Restores the terminal. Safe to call more than once.
fn restore(enhanced: bool) {
    let mut out = io::stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        cursor::Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        terminal::LeaveAlternateScreen
    );
    // The profile's background and the saved title, after leaving the alternate screen so the
    // shell's screen is repainted in its own colour.
    osc::restore(&mut out);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

fn install_panic_hook(enhanced: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(osc::restoring_hook(move || restore(enhanced), previous));
    // Ctrl-Break, the window closing, logoff: the console ends the process without unwinding, so
    // the panic hook never runs. Off Windows (and on failure) raw mode makes ^C a key instead.
    let _ = marlowe_os::console::on_interrupt(move || {
        notify_close();
        restore(enhanced);
    });
}

/// Whether the terminal can take escape sequences written as bytes. On Windows a console without
/// virtual-terminal processing would print them; crossterm switches it on where it exists.
fn speaks_ansi() -> bool {
    #[cfg(windows)]
    {
        crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Runs the UI until the user quits.
pub fn run(client: Client, mut state: State, theme: Theme) -> Result<(), TuiError> {
    let enhanced = !cfg!(windows) && matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    let closer = client.closer();
    let _ = CLOSE_NOTICE.set(Box::new({
        let closer = closer.clone();
        move || {
            closer.goodbye(&marlowe_proto::Request::WindowClosed, std::time::Duration::from_millis(1_500));
        }
    }));
    install_panic_hook(enhanced);
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, terminal::EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    if enhanced {
        // Lets terminals that support it report Shift+Enter distinctly.
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    // Marlowe's background, before the image probe asks the terminal what its background is.
    let ansi = speaks_ansi();
    if ansi {
        let keep = osc::keep_background(|k| std::env::var(k).ok());
        let _ = osc::enter(&mut out, keep);
    }
    // Before the input thread exists: the terminal's answers arrive as input.
    let images = start_images();
    // Waits of a few milliseconds (a frame at 144 Hz) are real ones, not rounded to 15.6 ms.
    let timer = marlowe_os::console::FineTimer::start();
    let result = event_loop(&client, &mut state, &theme, images, ansi);
    drop(timer);
    restore(enhanced);
    if result.is_ok() && state.farewell_shut() {
        // The daemon closed while it was on screen; the alternate screen is gone, so the last
        // word goes to the shell.
        println!("{}", state.goodnight());
    } else if result.is_ok() {
        // A clean goodbye: stop everything, or keep running if the user chose that.
        closer.goodbye(&state.exit_request(), std::time::Duration::from_millis(1_500));
    }
    result
}

/// Asks the terminal whether it can show Sixel images and, if `marlowe-typeset.exe` is beside this
/// executable, starts the maths image worker. The helper process itself starts on the first
/// formula. The receiver wakes the loop when an image is ready.
///
/// On by default, Windows Terminal included: an image is drawn to match the grid maths beside it
/// (its colour, a sans maths font at the terminal font's x-height, pixel-snapped to the cell the
/// terminal lays Sixel out on; see `marlowe-typeset-helper/src/raster.rs`).
///
/// `MARLOWE_MATH_IMAGES=0` keeps every formula on the character grid; `=1` skips the Sixel check
/// (for a terminal that supports it without saying so in DA1).
fn start_images() -> Option<Receiver<()>> {
    let setting = std::env::var("MARLOWE_MATH_IMAGES").unwrap_or_default();
    if setting == "0" {
        return None;
    }
    let helper = marlowe_typeset::helper::locate()?;
    use marlowe_typeset::probe;
    let reply = marlowe_os::console::query(probe::QUERIES, probe::complete, Duration::from_millis(400)).ok()?;
    let caps = probe::caps(&reply, setting == "1")?;
    let (tx, rx) = unbounded();
    let renderer = marlowe_typeset::helper::renderer(helper);
    marlowe_typeset::install(marlowe_typeset::Service::with_renderer(caps, renderer, tx));
    Some(rx)
}

fn event_loop(
    client: &Client,
    state: &mut State,
    theme: &Theme,
    images: Option<Receiver<()>>,
    ansi: bool,
) -> Result<(), TuiError> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    term.clear()?;
    let input = spawn_input();
    let (local_tx, local_rx) = unbounded::<ClientEvent>();
    let typeset = images.is_some();
    let images = images.unwrap_or_else(never);
    let mut emitter = Emitter::default();
    let mut links = LinkEmitter::default();
    let mut title = String::new();
    // A change is drawn the moment it arrives, at most once per display refresh: the first event
    // after a quiet spell draws at once and a burst becomes one frame ([`crate::pacing`]). Pointer
    // motion that lands on the same target as before changes nothing on screen and draws nothing.
    let mut pacer = Pacer::detect();
    let mut dirty = true;
    let mut drawn = Instant::now();
    loop {
        tick(state);
        send_all(client, state.on_tick());
        send_all(client, state.on_idle());
        run_effects(state, &local_tx, &mut term)?;
        pacer.set_cap(state.max_fps());
        let cadence = state.cadence();
        if pacer.until_draw(drawn.elapsed(), dirty, cadence).is_zero() {
            draw(&mut term, state, theme, typeset, &mut emitter, &mut links, ansi)?;
            drawn = Instant::now();
            dirty = false;
        }
        if ansi {
            let t = state.window_title();
            if t != title {
                let _ = osc::title(&mut io::stdout(), &t);
                title = t;
            }
        }
        if state.quit {
            // Stopping the daemon: keep showing its shutdown until it is done (or a key, or time).
            if let Some(r) = state.begin_farewell() {
                client.send(r);
            }
            if state.farewell_done() {
                return Ok(());
            }
        }
        let left = pacer.until_draw(drawn.elapsed(), dirty, cadence);
        // The clock is re-read before anything is applied: the approval type-ahead guard compares
        // a key's arrival with the modal's.
        select! {
            recv(client.events()) -> ev => match ev {
                Ok(ev) => {
                    tick(state);
                    send_all(client, state.on_client(ev));
                    dirty = true;
                }
                Err(_) => return Err(TuiError::Protocol("the connection thread stopped".into())),
            },
            recv(input) -> i => match i {
                Ok(i) => {
                    tick(state);
                    dirty |= handle(i, client, state, &term)?;
                }
                Err(_) => return Err(TuiError::Protocol("the input thread stopped".into())),
            },
            recv(local_rx) -> ev => {
                if let Ok(ev) = ev {
                    tick(state);
                    send_all(client, state.on_client(ev));
                    dirty = true;
                }
            },
            recv(images) -> _ => dirty = true,
            default(left) => {}
        }
        while images.try_recv().is_ok() {
            dirty = true;
        }
        // Apply everything already queued before the next frame.
        while let Ok(ev) = client.events().try_recv() {
            send_all(client, state.on_client(ev));
            dirty = true;
        }
        while let Ok(ev) = local_rx.try_recv() {
            send_all(client, state.on_client(ev));
            dirty = true;
        }
        while let Ok(i) = input.try_recv() {
            dirty |= handle(i, client, state, &term)?;
        }
    }
}

fn draw(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &State,
    theme: &Theme,
    typeset: bool,
    emitter: &mut Emitter,
    links: &mut LinkEmitter,
    ansi: bool,
) -> Result<(), TuiError> {
    // Every frame is one synchronised update (DEC mode 2026): the terminal shows it whole, with
    // its images and its links, never half-written while a reply streams. ratatui sends only the
    // cells that changed. A terminal without the mode ignores both sequences.
    let mut out = io::stdout();
    out.write_all(b"\x1b[?2026h")?;
    let frame = term.draw(|f| render::draw(f, state, theme))?;
    if typeset {
        emitter.flush(frame.buffer, &mut out)?;
    }
    if ansi {
        let plan = state.code.link_plan.borrow().clone();
        let bytes = links.frame(frame.buffer, &plan);
        if !bytes.is_empty() {
            out.write_all(&bytes)?;
        }
    }
    out.write_all(b"\x1b[?2026l")?;
    out.flush()?;
    Ok(())
}

/// Starts what the state asked for. The editor is run here, with the terminal handed over; the
/// rest go to worker threads.
fn run_effects(
    state: &mut State,
    tx: &Sender<ClientEvent>,
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), TuiError> {
    for e in state.take_effects() {
        match e {
            Effect::OpenEditor { path } => {
                let result = with_terminal_released(term, || local::run_editor(&path))?;
                let _ = tx.send(ClientEvent::Local(LocalEvent::EditorClosed { path, result }));
            }
            other => local::spawn(other, tx.clone()),
        }
    }
    Ok(())
}

/// Gives the terminal back (normal screen, cooked input, the cursor) while `f` runs, then takes it
/// again. The input thread stops reading so the editor gets every key.
fn with_terminal_released<T>(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    f: impl FnOnce() -> T,
) -> Result<T, TuiError> {
    INPUT_PAUSED.store(true, Ordering::SeqCst);
    // The input thread polls in 50 ms slices; let it notice.
    thread::sleep(Duration::from_millis(80));
    let mut out = io::stdout();
    execute!(out, DisableMouseCapture, DisableBracketedPaste, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    let r = f();
    terminal::enable_raw_mode()?;
    execute!(out, terminal::EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    // Whatever was typed into the editor's last moments is not for us.
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    INPUT_PAUSED.store(false, Ordering::SeqCst);
    term.clear()?;
    Ok(r)
}

fn tick(state: &mut State) {
    let now = chrono::Local::now();
    state.clock = now.format("%H:%M").to_string();
    state.date = now.format("%A %-d %B").to_string();
    state.now_ms = now.timestamp_millis();
}

fn send_all(client: &Client, reqs: Vec<marlowe_proto::Request>) {
    for r in reqs {
        client.send(r);
    }
}

/// Applies one input. Returns whether the screen may have changed: pointer motion that stays on
/// the same target does not.
fn handle(
    i: Input,
    client: &Client,
    state: &mut State,
    term: &Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<bool, TuiError> {
    match i {
        Input::Keys(keys) => {
            for k in keys {
                send_all(client, state.on_key(k));
            }
        }
        Input::Paste(text) => state.on_paste(&text),
        Input::Mouse(m) => {
            let size = term.size()?;
            let before = state.hover.clone();
            send_all(client, state.on_mouse(m, Rect::new(0, 0, size.width, size.height)));
            if m.kind == MouseEventKind::Moved {
                return Ok(state.hover != before);
            }
        }
        Input::Resize => {}
    }
    Ok(true)
}

fn spawn_input() -> Receiver<Input> {
    let (tx, rx) = unbounded();
    thread::Builder::new()
        .name("marlowe-tui-input".into())
        .spawn(move || loop {
            if INPUT_PAUSED.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            let ev = match event::read() {
                Ok(ev) => ev,
                Err(_) => return,
            };
            let msg = match ev {
                TermEvent::Key(k) if k.kind != KeyEventKind::Release => match read_burst(k) {
                    Ok(Burst::Paste(t)) => Input::Paste(t),
                    Ok(Burst::Keys(keys)) => Input::Keys(keys),
                    Err(_) => return,
                },
                TermEvent::Paste(t) => Input::Paste(t),
                TermEvent::Mouse(m) => Input::Mouse(m),
                TermEvent::Resize(..) => Input::Resize,
                _ => continue,
            };
            if tx.send(msg).is_err() {
                return;
            }
        })
        .expect("spawn input thread");
    rx
}

// Windows Terminal does not deliver bracketed paste through crossterm's console-API input path:
// a paste arrives as a fast run of key events, newlines as Enter. A run already queued behind
// the first key, carrying a newline or long enough, is treated as one paste.

/// Events in one run before a run without a newline counts as a paste.
const BURST_MIN: usize = 12;
const BURST_CAP: usize = 65_536;
/// How long to wait for the terminal to finish writing a chunked paste.
const GRACE_MS: u64 = 10;

enum Burst {
    Paste(String),
    Keys(Vec<KeyEvent>),
}

fn read_burst(first: KeyEvent) -> io::Result<Burst> {
    let mut keys = vec![first];
    'burst: loop {
        while keys.len() < BURST_CAP && event::poll(Duration::ZERO)? {
            match event::read()? {
                TermEvent::Key(k) if k.kind != KeyEventKind::Release => keys.push(k),
                TermEvent::Key(_) => {}
                _ => break 'burst,
            }
        }
        if keys.len() >= BURST_CAP || !event::poll(Duration::from_millis(GRACE_MS))? {
            break;
        }
    }
    Ok(classify(keys))
}

fn classify(keys: Vec<KeyEvent>) -> Burst {
    if keys.len() > 1 {
        let text: Option<String> = keys
            .iter()
            .map(|k| match k.code {
                KeyCode::Char(c) if k.modifiers.difference(event::KeyModifiers::SHIFT).is_empty() => Some(c),
                KeyCode::Enter => Some('\n'),
                KeyCode::Tab => Some('\t'),
                _ => None,
            })
            .collect();
        if let Some(text) = text {
            if text.contains('\n') || keys.len() >= BURST_MIN {
                return Burst::Paste(text);
            }
        }
    }
    Burst::Keys(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn bursts_with_newlines_are_pastes_and_single_keys_are_not() {
        assert!(matches!(classify(vec![KeyEvent::from(KeyCode::Enter)]), Burst::Keys(_)));
        match classify(vec![key('h'), key('i'), KeyEvent::from(KeyCode::Enter), key('x')]) {
            Burst::Paste(t) => assert_eq!(t, "hi\nx"),
            Burst::Keys(_) => panic!("a burst with a newline is a paste"),
        }
        assert!(matches!(classify(vec![key('h'), key('i')]), Burst::Keys(_)));
        let mut held: Vec<KeyEvent> = "hello world!".chars().map(key).collect();
        held.push(KeyEvent::from(KeyCode::Left));
        assert!(matches!(classify(held), Burst::Keys(_)));
    }
}
