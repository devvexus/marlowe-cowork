//! The terminal: raw mode, alternate screen, mouse, the input thread and the main loop.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{select, unbounded, Receiver};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyboardEnhancementFlags, MouseEvent,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor, execute, terminal};
use marlowe_surface::Theme;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::client::Client;
use crate::render;
use crate::state::State;
use crate::TuiError;

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
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

fn install_panic_hook(enhanced: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(enhanced);
        previous(info);
    }));
}

/// Runs the UI until the user quits.
pub fn run(client: Client, mut state: State, theme: Theme) -> Result<(), TuiError> {
    let enhanced = !cfg!(windows) && matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    install_panic_hook(enhanced);
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, terminal::EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    if enhanced {
        // Lets terminals that support it report Shift+Enter distinctly.
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    let result = event_loop(&client, &mut state, &theme);
    restore(enhanced);
    result
}

fn event_loop(client: &Client, state: &mut State, theme: &Theme) -> Result<(), TuiError> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    term.clear()?;
    let input = spawn_input();
    loop {
        tick(state);
        term.draw(|f| render::draw(f, state, theme))?;
        if state.quit {
            return Ok(());
        }
        // The boot and the voice meter move at ~30 fps; a turn at ~11; otherwise once a second.
        let wait = if state.booting() || state.wake.speaking() {
            33
        } else if state.animating() {
            90
        } else {
            1_000
        };
        // The clock is re-read before anything is applied: the approval type-ahead guard compares
        // a key's arrival with the modal's.
        select! {
            recv(client.events()) -> ev => match ev {
                Ok(ev) => {
                    tick(state);
                    send_all(client, state.on_client(ev))
                }
                Err(_) => return Err(TuiError::Protocol("the connection thread stopped".into())),
            },
            recv(input) -> i => match i {
                Ok(i) => {
                    tick(state);
                    handle(i, client, state, &term)?
                }
                Err(_) => return Err(TuiError::Protocol("the input thread stopped".into())),
            },
            default(Duration::from_millis(wait)) => {}
        }
        // Apply everything already queued before the next frame.
        while let Ok(ev) = client.events().try_recv() {
            send_all(client, state.on_client(ev));
        }
        while let Ok(i) = input.try_recv() {
            handle(i, client, state, &term)?;
        }
    }
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

fn handle(
    i: Input,
    client: &Client,
    state: &mut State,
    term: &Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), TuiError> {
    match i {
        Input::Keys(keys) => {
            for k in keys {
                send_all(client, state.on_key(k));
            }
        }
        Input::Paste(text) => state.on_paste(&text),
        Input::Mouse(m) => {
            let size = term.size()?;
            send_all(client, state.on_mouse(m, Rect::new(0, 0, size.width, size.height)));
        }
        Input::Resize => {}
    }
    Ok(())
}

fn spawn_input() -> Receiver<Input> {
    let (tx, rx) = unbounded();
    thread::Builder::new()
        .name("marlowe-tui-input".into())
        .spawn(move || loop {
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
