//! The terminal event loop. Owns raw mode, the alternate screen, and nothing else.
//!
//! Every decision that could be made without a terminal was made without one — focus, dispatch,
//! layout and drawing all live in `marlowe-surface` and are exercised headless. What is left here
//! is the part that genuinely needs a TTY, which is also the part that cannot be unit-tested and
//! therefore has to be small.
//!
//! # No real clock is read in this file
//!
//! Time comes from `marlowe_stub::Clock`, which is the fence `determinism_guard.rs` names. That is
//! not ceremony: it is what keeps every render a pure function of `(state, now_ms)`, which is what
//! makes §B13's flicker rows diffable.

use std::io::{self, Write};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::{execute, terminal};
use marlowe_stub::Clock;
use marlowe_view::Produce;
use marlowe_surface::app::{Action, App, Key};
use marlowe_surface::{render, Theme, MIN_COLS, MIN_ROWS};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// How often to repaint while the status indicator is sampling.
///
/// 20 fps. The meter is 12 cells; ratatui's differential renderer writes only the cells that
/// changed, so a tick that moves nothing else costs twelve cell writes. §B12's *"streaming does
/// not repaint the screen"* is a property of the diff, not of the tick rate — but the tick rate is
/// what decides how often the diff is even asked, and 50 ms is under the threshold where a level
/// meter reads as stepping rather than moving.
const ANIMATION_TICK_MS: u64 = 50;

/// Poll timeout when nothing is animating and nothing is scheduled. The loop blocks here; a
/// terminal doing nothing should cost nothing.
const IDLE_TICK_MS: u64 = 1_000;

pub struct Options {
    /// Print startup timings and the resolved capability tiers, then leave.
    pub timing_probe: bool,
    /// Render raw-mode state, colour tier and a live keystroke counter in the titlebar.
    ///
    /// A running TUI's most important properties are invisible from outside the process, and a
    /// separate probe run measures a *different* process. This makes the session itself answer.
    pub diagnostic: bool,
    /// Explicit colour-depth override. §B2's fallback chain is a probe, and a silent probe is the
    /// mismatch-hiding default this project has shipped four bugs behind — so its answer is
    /// printed, and this is how it is overridden.
    pub color_depth: Option<String>,
    /// **Experimental.** Ask the terminal to adopt the mockup's ground via OSC 10/11.
    ///
    /// §B2 says *never hardcode a background fill*, and this does not: there is no per-cell `bg`
    /// anywhere, `no_background_fill.rs` still passes, and focus is still signalled by border and
    /// label colour alone. OSC 11 changes **the terminal's own default background** — so the rule
    /// *"the terminal's background is the background"* stays literally true; the terminal is simply
    /// being asked to use the one the design was drawn against (`#0f0e14`), with `#cdc9d6` as the
    /// default foreground that §B2's weight-1 `Color::Reset` then resolves to.
    ///
    /// It is off by default and reverted on exit (OSC 110/111), because changing a user's terminal
    /// colours is exactly the kind of thing a program should ask for rather than assume. Whether it
    /// becomes the default, stays a flag, or is replaced by a shipped colour scheme is a decision
    /// this does not pre-empt — it exists so the choice can be made by looking at it.
    pub ground: bool,
    /// Drive M1's scripted stub instead of the real engine.
    ///
    /// **`--tui` is live by default and this is the opt-out, not the other way round.** The
    /// producer in use is printed at startup and named in the status band: ADR-029's rule
    /// generalised — the active producer is announced, never silently chosen, because an
    /// unannounced fallback is indistinguishable from the failure mode it resembles.
    pub scripted: bool,
    /// Point the client at a non-default daemon port.
    ///
    /// **Isolation, not configuration.** Two sessions on one machine share `DEFAULT_DAEMON_PORT`,
    /// and the reflex when one is in the way — stopping it — takes the other session's daemon with
    /// it. A scratch port is how a clean daemon is obtained without touching anyone else's.
    pub daemon_port: Option<u16>,
    /// **ADR-046's provider, forwarded to a daemon this process may have to spawn.**
    ///
    /// The TUI does not choose a provider — the daemon owns the run, and a `--provider` on a client
    /// invocation cannot change what an ALREADY-RUNNING daemon routes to. What this does is make
    /// the auto-spawn honest: `ensure_daemon` builds a fixed argv, so without this a
    /// `marlowe --tui --provider openrouter` on a machine with no daemon spawned a LOCAL one and
    /// answered from it, having parsed the flag and thrown it away.
    pub model_provider: marlowe_daemon::ModelProviderChoice,
    /// Enter raw mode and the alternate screen, draw one frame, then **panic on purpose**.
    ///
    /// The panic hook is the one piece of teardown that cannot be exercised by quitting normally,
    /// and it is also the one whose failure is most expensive: a panic inside the alternate screen
    /// with raw mode on and the mouse captured leaves a terminal that echoes nothing, selects
    /// nothing, and is still showing the alternate buffer — with the panic message invisible.
    ///
    /// This is a flag rather than a key so that nothing on the interactive surface can crash the
    /// application. It exists to be run, once, and to have its aftermath looked at.
    pub panic_probe: bool,
}

pub fn run(opts: Options) -> io::Result<()> {
    let clock = Clock::real();

    let theme = match Theme::resolve(|k| std::env::var(k).ok(), opts.color_depth.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            // A load-time refusal, not a fall-back to the default accent. See theme.rs.
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // **M2 C2d's deliverable: the TUI drives the real engine.** `--scripted` keeps M1's stub for
    // the interaction demo. Whichever is live is announced rather than inferred.
    if opts.scripted {
        let mut session = marlowe_stub::Session::new();
        return run_with(&mut session, opts, theme, clock, None);
    }
    // **Nothing here talks to a daemon.** §6: the header paints before the connection resolves,
    // and §B13 budgets 150 ms to it. The handshake happens after the first frame — see
    // `event_loop`'s first iteration — because a cold start otherwise shows a themed window with a
    // blinking cursor and nothing in it for as long as the daemon and Ollama take to wake up.
    let port = opts.daemon_port.unwrap_or(marlowe_daemon::DEFAULT_DAEMON_PORT);
    let mut session = marlowe_daemon::LiveSession::connecting("tui", port);
    run_with(&mut session, opts, theme, clock, Some(port))
}

/// Start a daemon if none is listening. **§5's zero-config first run**: the user types `marlowe`
/// (or clicks the shortcut) and it works.
///
/// **Announced, never silent** — a process appearing on a machine without the user being told is
/// exactly what a well-behaved tool does not do. It is spawned DETACHED rather than run in-process
/// because invariant 6 is the whole reason for the split: a daemon inside the TUI would die with
/// the terminal, which is the thing §6 exists to prevent.
/// What starting the daemon did. **Returned rather than printed.**
///
/// # The bug this exists for
///
/// These were `eprintln!`s, and `ensure_daemon` runs **after** the alternate screen is up — §B13
/// puts the first frame in front of the connection deliberately. So the message
/// `marlowe: no daemon running — starting one` was written straight onto the rendered frame,
/// past ratatui's buffer, and stayed there until something forced a full repaint. Reported as
/// *"all the stuff is weird and shows (no daemon running) until I resize the window"* — the resize
/// was not fixing a connection, it was erasing our own text.
///
/// Anything with something to say once the surface owns the terminal says it **through the view**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStart {
    /// One was already listening.
    AlreadyUp,
    /// We started one and it answered.
    Started,
    /// We could not start one at all.
    CouldNotSpawn,
    /// We started one and it did not answer in time. It may still come up.
    Slow,
}

/// The argv `ensure_daemon` spawns a daemon with, **as a value rather than as side effects on a
/// `Command`**, so the one property that matters can be asserted without spawning a process.
///
/// That property: **a daemon this process spawns is the daemon the flags described.** `--serve`,
/// `--ask` and `--status` all threaded `--provider`; `--tui` did not, and this function is where
/// the omission lived — a fixed argv that said `--serve --workspace <cwd>` and nothing else. So
/// `marlowe --tui --provider openrouter --openrouter-model X` parsed both flags, discarded them,
/// spawned a LOCAL daemon and answered from it. A flag accepted and ignored is worse than one
/// refused: nothing on screen says which model replied.
///
/// **The API key is deliberately NOT here.** It reaches the child through the inherited
/// environment. An argv is visible in every process listing on the machine, so a key passed this
/// way would be readable by any other user — see `marlowe_openrouter::secret`.
fn spawn_args(port: u16, provider: &marlowe_daemon::ModelProviderChoice) -> Vec<String> {
    let mut out = vec!["--serve".to_string()];
    if port != marlowe_daemon::DEFAULT_DAEMON_PORT {
        out.push("--daemon-port".to_string());
        out.push(port.to_string());
    }
    // **An exhaustive `match`, because the `if let` this replaces is the defect this function's
    // header records — repeated.** `marlowe --tui --provider llamacpp` parsed the flag, dropped
    // it, spawned a LOCAL OLLAMA daemon and answered from it, with nothing on screen saying which
    // runtime replied. A `match` makes the next provider a compile error here instead.
    match provider {
        marlowe_daemon::ModelProviderChoice::Ollama => {}
        marlowe_daemon::ModelProviderChoice::OpenRouter { model } => {
            out.push("--provider".to_string());
            out.push("openrouter".to_string());
            out.push("--openrouter-model".to_string());
            out.push(model.clone());
        }
        marlowe_daemon::ModelProviderChoice::LlamaCpp { endpoint, sampling } => {
            out.push("--provider".to_string());
            // The picker's own spelling, so what the TUI shows and what the daemon was told are
            // one string. `resolve_provider` accepts both, and this is the one a user reads.
            out.push(marlowe_view::provider::HYBRID.to_string());
            out.push("--llamacpp-port".to_string());
            out.push(endpoint.port().to_string());
            out.push("--llamacpp-sampling".to_string());
            out.push(
                match sampling {
                    marlowe_provider::llamacpp::SamplingSource::OllamaParams => "ollama",
                    marlowe_provider::llamacpp::SamplingSource::ServerDefaults => "server",
                }
                .to_string(),
            );
        }
    }
    out
}

fn ensure_daemon(port: u16, provider: &marlowe_daemon::ModelProviderChoice) -> DaemonStart {
    let client = marlowe_daemon::Client::new("tui").with_port(port);
    if client.daemon_is_up() {
        return DaemonStart::AlreadyUp;
    }
    let Ok(exe) = std::env::current_exe() else { return DaemonStart::CouldNotSpawn };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--workspace").arg(&cwd);
    for a in spawn_args(port, provider) {
        cmd.arg(a);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // **The daemon must not inherit this console, and nulling stdio is not enough.**
    //
    // Found by running it: `--timing-probe` returned in 56 ms and the shell hung anyway. A child
    // process on Windows inherits the parent's console handle, so a daemon that runs forever holds
    // the console open forever — the pipeline never sees end-of-output, and a terminal, a shell
    // pipeline or a double-clicked shortcut all appear to hang. Nothing on the Marlowe side is
    // wrong at that point; the process that finished simply cannot say so.
    //
    // DETACHED_PROCESS (0x8) gives the daemon no console at all, which is what a daemon should
    // have. CREATE_NEW_PROCESS_GROUP (0x200) keeps a Ctrl-C in the launching terminal from
    // reaching it — invariant 6 says the run outlives the client, and it would not survive
    // inheriting the client's interrupt.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    if cmd.spawn().is_err() {
        return DaemonStart::CouldNotSpawn;
    }
    // Wait for the socket rather than sleeping a fixed amount. §B13 budgets 150 ms to first frame
    // and a fixed sleep would spend it whether or not it was needed.
    // **Four seconds, not one.** The daemon opens a journal, takes a profile lock and builds an
    // engine before it binds; one second was enough on a warm run and not on a cold one, and
    // falling through left the band saying "no daemon" for a daemon that came up 200 ms later
    // with nothing ever retrying.
    for _ in 0..160 {
        // LOOP-EXEMPT: waiting on a socket to come up, not a driving loop.
        if client.daemon_is_up() {
            return DaemonStart::Started;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    DaemonStart::Slow
}

fn run_with(
    session: &mut impl Produce,
    opts: Options,
    theme: Theme,
    clock: Clock,
    connect_port: Option<u16>,
) -> io::Result<()> {
    let mut app = match App::new(session.view().clone()) {
        Ok(a) => a,
        Err(conflict) => {
            // §B2's contract, enforced before the first frame. A duplicate hotkey means one
            // region's bottom border is lying about how to reach it.
            eprintln!("error: {conflict}");
            std::process::exit(2);
        }
    };

    // §B11: request the minimum on start. Terminals that honour CSI 8 comply; those that do not
    // are handled by the refusal below, which is the same path a user who declines a resize takes.
    let mut out = io::stdout();
    write!(out, "\x1b[8;{MIN_ROWS};{MIN_COLS}t")?;
    out.flush()?;

    let (cols, rows) = terminal::size()?;
    if cols < MIN_COLS || rows < MIN_ROWS {
        // **Never a degraded grid.** One line naming current and required size, and the offer.
        // A narrow variant was designed and rejected: it cost the borders, which cost the region
        // contract, which is the entire design.
        eprintln!(
            "marlowe needs {MIN_COLS}x{MIN_ROWS}; this terminal is {cols}x{rows}.\n\
             Resize it, or run `marlowe --classic` — same commands, no grid."
        );
        std::process::exit(1);
    }

    // **The panic hook is installed BEFORE raw mode is entered, and it is not optional.**
    //
    // A panic inside the alternate screen with raw mode on leaves the user with a terminal that
    // echoes nothing, has no line discipline, and is still showing the alternate buffer — and the
    // panic message itself is invisible or mangled. `reset` is then the only way out.
    //
    // The hook restores the terminal FIRST and prints SECOND, so the message lands on a working
    // terminal. It chains to the previous hook rather than replacing it, so the backtrace survives.
    install_panic_hook();

    // An explicit `--color-depth` is an explicit request, and it outranks `NO_COLOR`.
    //
    // Honouring `NO_COLOR` by default is correct — it is a real convention and a user who set it
    // meant it. But a user who *also* passes `--color-depth truecolor` has asked twice, and
    // silently handing them a white screen would be the mismatch-hiding default again. So: the flag
    // forces colour on, and either way the startup record below says which happened.
    if theme.no_color().is_some() && opts.color_depth.is_some() {
        crossterm::style::force_color_output(true);
    }

    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        // §B10 amended (2026-08-08): the mouse is captured. Keyboard remains the primary and
        // complete path — every action is still reachable by key, and `b13_keyboard.rs` proves it
        // from the default focus. What capture buys is that drag-select stops belonging to the
        // terminal, which is the single thing that made a running application read as a printout.
        EnableMouseCapture,
        // Focus reporting is what lets hover clear when the pointer leaves the terminal entirely.
        event::EnableFocusChange,
        // **The terminal tells us it was a paste.** Without it a pasted block arrives as N key
        // events and the composer would have to guess from typing speed — a guess that fires on a
        // fast typist and is invisible to every test.
        event::EnableBracketedPaste,
        crossterm::cursor::Hide
    )?;

    if opts.ground {
        // OSC 11 = default background, OSC 10 = default foreground. The mockup's `--bg` and
        // `--text`. BEL-terminated, which every emulator accepts; ST-terminated is stricter but
        // less widely handled.
        let mut o = io::stdout();
        write!(o, "\x1b]11;#0f0e14\x07\x1b]10;#cdc9d6\x07")?;
        o.flush()?;
    }

    // Read back rather than assume. `enable_raw_mode` returning `Ok` means the call succeeded, not
    // that the mode is in force — under a redirected stdout or a console host that does not
    // support it, those are different things, and the difference is invisible until a keystroke
    // fails to arrive.
    let raw_mode = terminal::is_raw_mode_enabled().unwrap_or(false);

    if opts.diagnostic {
        app.diagnostic = Some(format!(
            "raw={} alt=on mouse=captured tier={} {}",
            if raw_mode { "ON" } else { "OFF" },
            theme.depth().as_str(),
            // NOT `depth_reason` alone. A tier printed next to a screen with no colour on it is a
            // record that agrees with the code and disagrees with the user.
            match theme.no_color() {
                Some(v) if opts.color_depth.is_some() => format!("NO_COLOR={v} OVERRIDDEN"),
                Some(v) => format!("NO_COLOR={v} SUPPRESSES ALL COLOUR"),
                None => theme.depth_reason().to_string(),
            },
        ));
    }

    // Installed before the loop, so a window closed at any point after this is covered.
    #[cfg(windows)]
    if let Some(port) = connect_port {
        close_handler::install(port);
    }

    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut term, session, &mut app, &theme, &clock, &opts, connect_port);
    // **Read before the teardown, because a turn in flight changes what closing means.**
    let was_mid_turn = session.is_busy();

    if opts.ground {
        // OSC 110/111 reset fore/background to the terminal's configured defaults. A program that
        // changes a user's terminal colours and does not change them back is a program they will
        // not run twice.
        let mut o = io::stdout();
        write!(o, "\x1b]111\x07\x1b]110\x07")?;
        o.flush()?;
    }

    execute!(
        io::stdout(),
        crossterm::cursor::Show,
        DisableMouseCapture,
        event::DisableFocusChange,
        event::DisableBracketedPaste,
        terminal::LeaveAlternateScreen
    )?;
    terminal::disable_raw_mode()?;

    // **Closing the window stops the daemon.** Invariant 6 says a RUN survives the client that
    // started it, not that the daemon is immortal — and with no way to stop one, a closed window
    // left a process listening that the next launch silently reconnected to. That is how a fixed
    // build gets tested against a stale one, which happened three times in one session.
    //
    // The daemon refuses while a run is in flight, which is the case the invariant is actually
    // about. Printed here rather than in the band because the alternate screen is already down.
    //
    // **Not while a turn is in flight, and the CLIENT is the only one who can tell.** The daemon
    // is serial: a shutdown request sent during a turn waits in the accept backlog until that turn
    // finishes, and by then the run is marked complete — so the daemon's own "refuse while a run
    // is live" check inspects a run table that is already quiet and always agrees to stop.
    //
    // Reproduced: hang up three reasoning deltas into a turn, send shutdown, and the daemon
    // finishes the turn and then exits, taking the conversation with it. Reopening gave a fresh
    // session, which is why closing mid-reason and coming back looked like Marlowe had lost his
    // mind — he had lost the conversation.
    //
    // Invariant 6 is the rule and this is what honouring it looks like from the client side: if
    // work was running when the window closed, the daemon keeps running and the conversation is
    // there on the way back in.
    if let Some(port) = connect_port.filter(|_| !was_mid_turn) {
        let client = marlowe_daemon::Client::new("tui").with_port(port);
        match client.shutdown() {
            Ok(events) => {
                for e in events {
                    if let marlowe_daemon::Event::Error { detail } = e {
                        println!("marlowe: daemon still running — {detail}");
                    }
                }
            }
            // Already gone, or never ours. Neither is worth a line.
            Err(_) => {}
        }
    }

    if let Some((first_frame_ms, interactive_ms)) = result? {
        println!("raw_mode                {}", on_off(raw_mode));
        println!("alternate_screen        on");
        // §B10: *keyboard first; mouse is a bonus.* Not capturing the mouse is a decision, not an
        // omission: with mouse reporting off, the terminal's own selection and copy keep working,
        // which is what a user expects of a program they may want to copy a line out of. The cost
        // is that click-to-focus does not exist, and every path is therefore reachable by key.
        // §B10 amended 2026-08-08: captured. Keyboard is still the primary and complete path —
        // every action remains reachable by key — but selection no longer belongs to the terminal.
        println!("mouse_capture           on (§B10 as amended; keyboard remains complete)");
        println!("color_depth             {}", theme.depth().as_str());
        println!("color_depth_reason      {}", theme.depth_reason());
        println!("color_emitted           {}", theme.emission_report());
        println!("accent                  {}", theme.accent_source());
        println!("terminal                {cols}x{rows}");
        println!("time_to_first_frame_ms  {first_frame_ms}");
        println!("time_to_interactive_ms  {interactive_ms}");
        println!("target_first_frame_ms   150   (K4)");
        println!("target_interactive_ms   300");
    }
    Ok(())
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "OFF — keystrokes will not reach the event loop"
    }
}

/// Restore the terminal on the way out of a panic, then let the default hook print.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Mouse capture is released here too. A panic that leaves capture on hands the user a
        // terminal whose mouse no longer selects text and no longer scrolls — with no application
        // running to explain why. It is the same class of damage as leaving raw mode on.
        let _ = execute!(
            io::stdout(),
            crossterm::cursor::Show,
            DisableMouseCapture,
            event::DisableFocusChange,
            event::DisableBracketedPaste,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
        previous(info);
    }));
}

/// Returns `Some((first_frame_ms, interactive_ms))` under `--timing-probe`.

/// **Closing the window with the X, on Windows.**
///
/// The teardown at the bottom of [`run`] only executes if `event_loop` returns. Clicking the close
/// button does not unwind the process — Windows raises `CTRL_CLOSE_EVENT` on a handler thread and
/// then terminates it — so the shutdown never ran and the detached daemon outlived its client.
/// Observed 2026-08-10: the next `--status` reported *"this daemon's binary is 17 min older than
/// the source it was built from"*, which is exactly the stale-daemon trap this project has paid
/// for three times.
///
/// A control handler is the only place code can run at that moment. Windows gives it a few seconds
/// before killing the process, which is ample for one loopback request.
///
/// **The busy check is preserved.** [`TURN_IN_FLIGHT`] mirrors `session.is_busy()`, so a window
/// closed mid-turn leaves the daemon running and the conversation intact — the same rule the
/// graceful path follows, for the same reason (invariant 6).
#[cfg(windows)]
mod close_handler {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// The daemon's port, or 0 when this TUI did not connect to one.
    static PORT: AtomicU32 = AtomicU32::new(0);
    /// Whether a turn is running. A window closed mid-turn must not stop the daemon.
    pub static TURN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    // `kernel32` is already linked by std on Windows, so this needs no new dependency. One
    // function, declared by hand rather than pulling in a bindings crate for it.
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    unsafe extern "system" fn on_console_event(event: u32) -> i32 {
        if !matches!(
            event,
            CTRL_C_EVENT
                | CTRL_BREAK_EVENT
                | CTRL_CLOSE_EVENT
                | CTRL_LOGOFF_EVENT
                | CTRL_SHUTDOWN_EVENT
        ) {
            return 0;
        }
        let port = PORT.load(Ordering::Relaxed);
        if port != 0 && !TURN_IN_FLIGHT.load(Ordering::Relaxed) {
            // Best effort. The process is about to die either way, and a daemon left running is
            // the failure this exists to prevent rather than one it can cause.
            let client = marlowe_daemon::Client::new("tui").with_port(port as u16);
            let _ = client.shutdown();
        }
        // TRUE: handled. Windows terminates the process next regardless.
        1
    }

    /// Install the handler. Called once, only when a port is known.
    pub fn install(port: u16) {
        PORT.store(port as u32, Ordering::Relaxed);
        // SAFETY: one FFI call into kernel32 with a `'static` function pointer and no data
        // shared across the boundary. The handler touches only atomics and a fresh client.
        unsafe {
            SetConsoleCtrlHandler(Some(on_console_event), 1);
        }
    }
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session: &mut impl Produce,
    app: &mut App,
    theme: &Theme,
    clock: &Clock,
    opts: &Options,
    connect_port: Option<u16>,
) -> io::Result<Option<(u64, u64)>> {
    advance(session, app, clock.now_ms());
    term.draw(|f| render::draw(app, theme, f.area(), f.buffer_mut()))?;
    let first_frame_ms = clock.now_ms();

    // **The frame is up; now talk to the daemon.** Spawning it and doing the `Status` round-trip
    // costs whatever a cold Ollama costs, and none of it is in front of the first paint.
    if let Some(port) = connect_port {
        let start = ensure_daemon(port, &opts.model_provider);
        session.connect_now();
        app.update(session.view().clone());
        // Said through the band, never through stdout: the surface owns this terminal now.
        if let Some(note) = match start {
            DaemonStart::Started => Some("started the daemon; it outlives this window"),
            DaemonStart::CouldNotSpawn => Some("could not start a daemon — run `marlowe --serve`"),
            DaemonStart::Slow => Some("the daemon is still starting"),
            DaemonStart::AlreadyUp => None,
        } {
            app.set_status_detail(note);
        }
        term.draw(|f| render::draw(app, theme, f.area(), f.buffer_mut()))?;
    }

    // The window title, kept current with session state via OSC 0. Tracked rather than re-emitted
    // every frame: at the animation tick that would be twenty title writes a second, and some
    // terminals flash the taskbar entry on each one.
    let mut title = String::new();

    // "Interactive" is the first moment a keystroke would be accepted, which is the first
    // completed poll — not the moment the loop is entered. Measuring the latter would report a
    // number that is true of the code and false of the user.
    event::poll(std::time::Duration::from_millis(0))?;
    let interactive_ms = clock.now_ms();

    if opts.timing_probe {
        return Ok(Some((first_frame_ms, interactive_ms)));
    }

    if opts.panic_probe {
        // Deliberate. The hook installed before raw mode must restore the terminal and then print,
        // in that order, or the message lands on a terminal that cannot show it.
        panic!("--panic-probe: this panic is intentional; the terminal should be usable below");
    }

    loop {
        let now = clock.now_ms();
        advance(session, app, now);
        // Mirrored for the close handler, which cannot reach `session`.
        #[cfg(windows)]
        close_handler::TURN_IN_FLIGHT.store(session.is_busy(), std::sync::atomic::Ordering::Relaxed);

        let timeout = if app.view().status.state.samples_amplitude() {
            ANIMATION_TICK_MS
        } else {
            session
                .next_beat_in(now)
                .unwrap_or(IDLE_TICK_MS)
                .clamp(1, IDLE_TICK_MS)
        };

        if event::poll(std::time::Duration::from_millis(timeout))? {
            match event::read()? {
                // **A burst of queued key events is a paste**, on the platform whose terminal
                // will not say so. Pasting three paragraphs here SENT THREE MESSAGES, because
                // each embedded newline arrived as `Enter`. See `keyburst`; `Event::Paste`
                // below still serves the platforms that do deliver it.
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    match crate::keyburst::read_burst(k)? {
                        crate::keyburst::Burst::Paste(text) => app.paste(text),
                        crate::keyburst::Burst::Keys(keys) => {
                            // Every key, in order. A burst that was not a paste loses nothing.
                            for k in keys {
                                if let Some(key) = translate(k.code, k.modifiers) {
                                    if app.on_key(key) == Action::Quit {
                                        return Ok(None);
                                    }
                                }
                            }
                        }
                    }
                }
                // §B10 as amended: the mouse is captured, so these events arrive here instead of
                // being handled by the terminal. Every one of them routes through the same
                // dispatch the keyboard uses — the mouse adds no state and no second code path,
                // which is what keeps "reachable by keyboard alone" true rather than merely
                // asserted.
                Event::Mouse(m) => {
                    app.mouse_seen += 1;
                    // **The same derivation the draw uses.** `render::layout` is the resting
                    // height; with a grown composer the click targets would sit where the borders
                    // used to be.
                    let chrome = render::chrome_for(app, term.size()?.into());
                    let hit = region_at(app, &chrome, m.column, m.row);
                    // Every motion event updates hover, including the ones that leave a region for
                    // dead chrome — that is the half that makes a hover clear rather than stick.
                    app.set_hover(hit);
                    // And the option under the pointer inside an open dropdown, which is not a
                    // region and so cannot ride on `hover`.
                    app.hover_option = app
                        .open_picker()
                        .and_then(|(_, i, n)| option_at(&chrome, i, n, term.size().ok()?, m));
                    // And the inspector tab, which is not a region either.
                    app.hover_tab = tab_at(&chrome, m.column, m.row);
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            // An open dropdown takes the click first, exactly as it takes the
                            // arrows and Enter from the keyboard (§B10, dispatch step 3).
                            if let Some((id, i, n)) = app.open_picker() {
                                let size = term.size()?;
                                app.focus = id;
                                if let Some(opt) = option_at(&chrome, i, n, size, m) {
                                    app.choose_option(opt);
                                } else {
                                    // Outside the popup: dismiss without choosing.
                                    app.on_key(Key::Esc);
                                    // **And if the click landed on ANOTHER control cell, open that
                                    // one in the same click.** Requiring a first click to dismiss
                                    // and a second to open is the behaviour of a modal nobody
                                    // asked for; every real menu bar switches on one click.
                                    if let Some(other) = hit {
                                        if other != id && App::STRIP.contains(&other) {
                                            app.focus = other;
                                            app.open_focused_picker();
                                        }
                                    }
                                }
                            } else if let Some(tab) = tab_at(&chrome, m.column, m.row) {
                                // Tabs are selectable by pointer as well as by digit. Routed
                                // through the same key the tab bar advertises, so there is one
                                // switch path rather than two.
                                app.on_key(Key::Char(tab.digit()));
                            } else if let Some(id) = hit {
                                app.focus = id;
                                // Clicking a control cell opens it. Focusing without opening is
                                // the state a user reads as "the click did nothing".
                                app.open_focused_picker();
                            }
                        }
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            // Focus follows the wheel, then the ordinary arrow-key path does the
                            // work. Scrolling a region you are not focused on would put the
                            // scrollbar and the focus ring in disagreement.
                            // The inspector is not a region — it has no border of its own (§B2) —
                            // so the wheel over it moves the pane directly rather than through a
                            // focus change.
                            let over_inspector = m.column >= chrome.inspector.x
                                && m.column < chrome.inspector.x + chrome.inspector.width
                                && m.row >= chrome.inspector.y
                                && m.row < chrome.inspector.y + chrome.inspector.height;
                            // **Over the inspector the wheel scrolls the PANE, whether or not it
                            // is over an item.** Items are hit-testable now, so `hit.is_none()`
                            // stopped being true exactly where scrolling matters most — a pane
                            // full of runs — and the wheel would have started nudging focus
                            // between rows instead of moving the list. Scrolling a list is what a
                            // wheel over a list means.
                            if over_inspector {
                                let max = app.inspector_scroll_max.get();
                                app.inspector_scroll = if m.kind == MouseEventKind::ScrollUp {
                                    app.inspector_scroll.saturating_sub(1)
                                } else {
                                    (app.inspector_scroll + 1).min(max)
                                };
                                advance(session, app, clock.now_ms());
                                term.draw(|f| {
                                    render::draw(app, theme, f.area(), f.buffer_mut())
                                })?;
                                continue;
                            }
                            if let Some(id) = hit {
                                app.focus = id;
                            }
                            let key = if m.kind == MouseEventKind::ScrollUp {
                                Key::Up
                            } else {
                                Key::Down
                            };
                            app.on_key(key);
                        }
                        _ => {}
                    }
                }
                // The pointer leaving the window delivers no motion event, so without this the
                // highlight would stay lit under a pointer that is now in another application.
                // A stuck hover is worse than no hover.
                // One event for the whole block. §B10's composer, and the reason a paragraph is
                // pasteable at all — see `App::paste`.
                Event::Paste(text) => {
                    app.paste(text);
                }
                Event::FocusLost => {
                    app.set_hover(None);
                }
                Event::Resize(_, _) => {
                    // ratatui reflows on the next draw. Nothing to recompute here, which is the
                    // point of layout being a pure function of the area.
                }
                _ => {}
            }
        }

        // §B10's copy. The payload was built by the surface; writing it needs stdout, which is
        // this file's job. OSC 52 goes straight out rather than through ratatui's buffer — it is
        // not a cell, and queueing it would tie a clipboard write to a repaint.
        if let Some(text) = app.pending_copy.take() {
            let mut out = io::stdout();
            write!(out, "{}", marlowe_surface::clipboard::osc52(&text))?;
            out.flush()?;
        }

        let next_title = app.window_title();
        if next_title != title {
            title = next_title;
            let mut out = io::stdout();
            write!(out, "\x1b]0;{title}\x07")?;
            out.flush()?;
        }

        advance(session, app, clock.now_ms());
        term.draw(|f| render::draw(app, theme, f.area(), f.buffer_mut()))?;
    }
}

/// One turn of the crank: hand the surface's requests to the producer, advance it, republish.
///
/// **This function is the client/daemon boundary, in miniature.** Every change the user causes
/// goes out as an `Intent` and comes back as a whole new view; nothing on the surface's side is
/// edited in place. A refusal is printed rather than dropped -- a producer that silently ignored
/// an intent would make a working build and a broken one look identical.
fn advance(session: &mut impl Produce, app: &mut App, now_ms: u64) {
    for intent in app.drain_intents() {
        if let Err(e) = session.apply(intent) {
            // Shown in the conversation and kept there. A refusal that vanished on the next
            // keystroke would leave a blocked action with no explanation.
            app.refused(&e);
        }
    }
    serve_window_asks(app);
    session.tick(now_ms);
    app.update(session.view().clone());
}

/// `/watch` and `/steer`, which the **driver** performs. `M3-DESIGN.md` §6.6.
///
/// # Why these are not `Intent`s
///
/// An `Intent` is something the *producer* — the session — applies. Neither of these is: opening a
/// window is a process spawn, and steering reaches the control plane of a run this session may not
/// even have started. Routing them through the producer would put a socket and a `Command` behind
/// `Produce`, which is a trait `marlowe-stub` also implements with neither.
///
/// **`/watch` never streams into the conversation pane**, which is §6.6's first rule: filling the
/// main pane with agent output halts the conversation *visually*, and that is what this milestone
/// exists to stop. What lands here is one client line saying where the window went — or, when a
/// window could not be opened, the command that attaches from anywhere.
fn serve_window_asks(app: &mut App) {
    use marlowe_surface::commands::Outcome;
    use marlowe_view::{CommandLine, Echo, Notice, Terminal, Tone};

    for ask in app.drain_window_asks() {
        match ask {
            Outcome::Watch(run) => {
                let out = crate::launcher::open_window(&run);
                if let Some(why) = &out.degraded {
                    // The reason is the launcher's own — a missing `wt.exe`, an unsupported
                    // platform — and it is shown before the command rather than instead of it.
                    app.set_status_detail(why.clone());
                }
                app.note(
                    Notice::WindowOpened {
                        run: Echo::new(run),
                        // A closed set: the harness only names terminals it knows how to drive.
                        terminal: out
                            .terminal
                            .as_deref()
                            .and_then(|t| (t == "Windows Terminal").then_some(Terminal::WindowsTerminal)),
                        command: CommandLine::new(out.command),
                    },
                    Tone::Normal,
                );
            }
            _ => {}
        }
    }
}

/// Which inspector tab a pointer is over. Geometry from `render::tab_rects`, never recomputed.
fn tab_at(chrome: &render::Chrome, col: u16, row: u16) -> Option<marlowe_view::Tab> {
    render::tab_rects(chrome.tab_bar)
        .into_iter()
        .find(|(_, r)| col >= r.x && col < r.x + r.width && row == r.y)
        .map(|(t, _)| t)
}

/// Which dropdown option a pointer is over, or `None` if it is not inside the popup's option rows.
///
/// Shared by hover and click so the highlighted row and the chosen row can never be different ones
/// — the failure that reads as flakiness rather than as a bug. The popup carries a border, so the
/// first option is one row inside its own top edge.
fn option_at(
    chrome: &render::Chrome,
    strip_index: usize,
    options: usize,
    size: ratatui::layout::Size,
    m: crossterm::event::MouseEvent,
) -> Option<usize> {
    let popup = render::dropdown_rect(chrome.control[strip_index], options, size.height);
    let inside = m.column >= popup.x
        && m.column < popup.x + popup.width
        && m.row > popup.y
        && m.row < popup.y + popup.height.saturating_sub(1);
    inside.then(|| (m.row - popup.y - 1) as usize)
}

/// Which region contains a screen cell, or `None` for the chrome between them.
///
/// Deliberately does **not** resolve inspector items: a click lands on the inspector, and the item
/// within it is chosen by key. Hit-testing individual items would be the first place the mouse grew
/// a capability the keyboard did not have, and §B10 keeps them at parity.
/// Which region a point is inside.
///
/// # §B10 said hit-testing stops at the region, and an inspector item IS one
///
/// That sentence — *"individual inspector items are chosen by key, because item-level hit-testing
/// would be the first place the mouse grew a path of its own"* — was written before items became
/// regions. `RegionTree::build` gives each one a `RegionId`, a border and a hotkey, and §B2's
/// focus table styles it exactly like every other region. So *"click focuses a region"* already
/// covers it.
///
/// **The prohibition's stated REASON is what is honoured here, not its letter.** A click on an item
/// does precisely what its letter does — one focus change, through the same dispatch — so the
/// mouse gains no capability the keyboard lacks and no second state path. The thing §B10 was
/// guarding against is a mouse that can reach somewhere the keyboard cannot, and this is the
/// opposite: with more than seventeen runs the letters run out (see `pane_key`) and the pointer is
/// the only way left to reach the eighteenth.
fn region_at(
    app: &App,
    chrome: &render::Chrome,
    col: u16,
    row: u16,
) -> Option<marlowe_surface::region::RegionId> {
    use marlowe_surface::region::RegionId;
    let hit = |r: ratatui::layout::Rect| {
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    };
    // Items first: they sit inside the inspector's rect, so a later `chrome.inspector` test would
    // shadow them.
    for (i, r) in render::item_rects(app, chrome) {
        if hit(r) {
            return Some(RegionId::Item(app.tab(), i));
        }
    }
    for (i, id) in [
        RegionId::Model,
        RegionId::Profile,
        RegionId::Session,
        RegionId::Workspace,
        RegionId::Autonomy,
    ]
    .into_iter()
    .enumerate()
    {
        if hit(chrome.control[i]) {
            return Some(id);
        }
    }
    if hit(chrome.status) {
        return Some(RegionId::Status);
    }
    if hit(chrome.conversation) {
        return Some(RegionId::Conversation);
    }
    if hit(chrome.message) {
        return Some(RegionId::Message);
    }
    None
}

/// crossterm → the surface's key vocabulary.
///
/// The translation lives here so `marlowe-surface::app` never imports crossterm, and every
/// keyboard property §B13 asks for is testable without a terminal.
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
        // §B10: multiline by default — Shift-Enter is a newline, Enter sends. Terminals that do
        // not distinguish them send plain Enter, which sends; that is a terminal limitation and
        // is not worked around by making Enter ambiguous.
        KeyCode::Enter if shift => Key::ShiftEnter,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    })
}

#[cfg(test)]
mod spawn_argv {
    use super::*;
    use marlowe_daemon::ModelProviderChoice;

    /// **The regression this closes**, asserted on the argv rather than on a spawned process.
    #[test]
    fn the_tui_forwards_the_provider_to_a_daemon_it_spawns() {
        let a = spawn_args(
            marlowe_daemon::DEFAULT_DAEMON_PORT,
            &ModelProviderChoice::OpenRouter { model: "stealth/ox-alpha".into() },
        );
        let joined = a.join(" ");
        assert!(joined.contains("--provider openrouter"), "{joined}");
        assert!(joined.contains("--openrouter-model stealth/ox-alpha"), "{joined}");
        assert!(a.contains(&"--serve".to_string()), "{joined}");
    }

    /// The control. Without it, the assertion above would pass on a build that always appended the
    /// flags, and "the provider is forwarded" would be a statement about a constant.
    #[test]
    fn an_ollama_spawn_carries_no_provider_flags_at_all() {
        let a = spawn_args(marlowe_daemon::DEFAULT_DAEMON_PORT, &ModelProviderChoice::Ollama);
        assert_eq!(a, vec!["--serve".to_string()], "the default spawn must be unchanged: {a:?}");
    }

    /// **The regression for the hybrid, asserted the same way.** ADR-060 makes
    /// `ollama/llama.cpp` opt-in; a `--tui` that dropped the flag would spawn a daemon on the
    /// DEFAULT provider while the client believed otherwise — Ollama answering while the TUI shows
    /// llama.cpp, which is the OpenRouter defect this module exists for and the worst of the three
    /// sites ADR-060 had to close.
    #[test]
    fn the_tui_forwards_a_hybrid_choice_including_the_port_and_the_sampler() {
        let a = spawn_args(
            marlowe_daemon::DEFAULT_DAEMON_PORT,
            &ModelProviderChoice::LlamaCpp {
                endpoint: marlowe_provider::LocalEndpoint::new("127.0.0.1", 11999).unwrap(),
                sampling: marlowe_provider::llamacpp::SamplingSource::ServerDefaults,
            },
        );
        let joined = a.join(" ");
        // **The picker's own spelling.** `resolve_provider` accepts `llamacpp` as an alias, so a
        // build emitting the alias here would still work — and the child's `--status` would then
        // report a name the parent's picker does not offer. One string, both sides.
        assert!(
            joined.contains(&format!("--provider {}", marlowe_view::provider::HYBRID)),
            "{joined}"
        );
        // The port and the sampler are not decoration: a child spawned on the default port would
        // probe a server nobody started, and one spawned on the default sampler would run the
        // model at Ollama's temperature after the user asked for llama.cpp's.
        assert!(joined.contains("--llamacpp-port 11999"), "{joined}");
        assert!(joined.contains("--llamacpp-sampling server"), "{joined}");
    }

    /// A non-default port still reaches the child, and does so alongside the provider rather than
    /// instead of it — the two are independent and a reader should not have to assume that.
    #[test]
    fn a_scratch_port_and_a_provider_both_survive() {
        let a = spawn_args(
            11500,
            &ModelProviderChoice::OpenRouter { model: "stealth/ox-alpha".into() },
        );
        let joined = a.join(" ");
        assert!(joined.contains("--daemon-port 11500"), "{joined}");
        assert!(joined.contains("--openrouter-model stealth/ox-alpha"), "{joined}");
    }

    /// **The key must never be in an argv.** A process listing is world-readable on this machine.
    #[test]
    fn no_spawn_argv_can_contain_a_key() {
        for p in [
            ModelProviderChoice::Ollama,
            ModelProviderChoice::OpenRouter { model: "stealth/ox-alpha".into() },
            ModelProviderChoice::LlamaCpp {
                endpoint: marlowe_provider::llamacpp::default_endpoint(),
                sampling: marlowe_provider::llamacpp::SamplingSource::OllamaParams,
            },
        ] {
            let joined = spawn_args(11500, &p).join(" ");
            assert!(!joined.contains("sk-or"), "a key reached the argv: {joined}");
            assert!(!joined.to_lowercase().contains("api_key"), "{joined}");
        }
    }
}
