//! **Nothing writes to the terminal by hand while the surface owns it.**
//!
//! # The bug
//!
//! Reported live: *"when I first open the app all the stuff is weird and shows (no daemon running)
//! — until I resize the window, then everything looks fine."*
//!
//! It was not a connection problem. §B13 puts the first frame **in front of** the daemon
//! handshake deliberately, so `ensure_daemon` runs after `EnterAlternateScreen` — and it called
//! `eprintln!("marlowe: no daemon running — starting one…")`. That went straight to the terminal,
//! past ratatui's buffer, and landed on top of the rendered frame. ratatui redraws only the cells
//! it believes changed, so the stray text survived every subsequent frame. A resize invalidates
//! the whole buffer, which is why resizing "fixed" it: the resize was erasing our own text.
//!
//! # Why this is a source scan
//!
//! The honest check would be a real terminal with a real daemon-spawn, and this project has
//! written down what a headless buffer does not prove. But the failure here is **not** a rendering
//! bug that a buffer test could miss — it is a byte written to a file descriptor that ratatui does
//! not own, and the only way that byte gets written is a print macro in this region of this file.
//! Scanning for it catches reintroduction at the point where it is cheap.
//!
//! The negative control below is what keeps it from being decorative.

const TUI: &str = include_str!("../src/tui.rs");

/// The span during which the surface owns the terminal.
fn alternate_screen_region(src: &str) -> (usize, usize) {
    let enter = src
        .find("terminal::EnterAlternateScreen")
        .expect("the TUI enters the alternate screen; if this moved, this guard is stale");
    let leave = src
        .find("terminal::LeaveAlternateScreen")
        .expect("the TUI leaves the alternate screen; if this moved, this guard is stale");
    assert!(enter < leave, "enter must precede leave in the source");
    (enter, leave)
}

/// Print macros in `src`, as (line number, line), ignoring comments and doc comments.
fn print_macros(src: &str) -> Vec<(usize, String)> {
    src.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .filter(|(_, l)| !l.starts_with("//"))
        .filter(|(_, l)| l.contains("println!") || l.contains("eprintln!") || l.contains("print!"))
        .collect()
}

#[test]
fn nothing_prints_to_the_terminal_while_the_alternate_screen_is_up() {
    let (enter, leave) = alternate_screen_region(TUI);
    let enter_line = TUI[..enter].lines().count();
    let leave_line = TUI[..leave].lines().count();

    let offenders: Vec<(usize, String)> = print_macros(TUI)
        .into_iter()
        .filter(|(n, _)| *n > enter_line && *n < leave_line)
        .collect();

    assert!(
        offenders.is_empty(),
        "a print macro runs while the surface owns the terminal (lines {enter_line}..{leave_line}).\n\
         The bytes go past ratatui's buffer and sit on top of the rendered frame until something \
         forces a full repaint — observed as a corrupted first screen that a window resize \
         'fixed'. Say it through the view instead: `App::set_status_detail`, or a `Notice`.\n\
         {offenders:#?}"
    );
}

/// **The negative control.** Without it the test above passes on any file with no prints at all,
/// including one where the region could not be found — and a guard that cannot fail is a comment.
#[test]
fn the_scan_would_catch_a_print_inside_the_region() {
    let fake = "\
fn main() {
    execute!(terminal::EnterAlternateScreen);
    eprintln!(\"marlowe: no daemon running\");
    execute!(terminal::LeaveAlternateScreen);
    println!(\"this one is fine\");
}
";
    let (enter, leave) = alternate_screen_region(fake);
    let enter_line = fake[..enter].lines().count();
    let leave_line = fake[..leave].lines().count();
    let offenders: Vec<(usize, String)> = print_macros(fake)
        .into_iter()
        .filter(|(n, _)| *n > enter_line && *n < leave_line)
        .collect();

    assert_eq!(offenders.len(), 1, "the eprintln inside the region must be caught: {offenders:?}");
    assert!(offenders[0].1.contains("no daemon running"));
}

/// The real file still has prints **outside** the region — the `--timing-probe` report and the
/// startup errors — so the scan is looking at a file where the distinction matters.
#[test]
fn the_file_under_test_does_print_elsewhere() {
    assert!(
        !print_macros(TUI).is_empty(),
        "no print macros at all in tui.rs; the guard is scanning something that cannot fail"
    );
}
