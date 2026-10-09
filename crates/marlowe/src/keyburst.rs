//! **Recognising a paste on a platform that will not tell us one happened.**
//!
//! # The control that nothing read
//!
//! `EnableBracketedPaste` is emitted by both surfaces, and `Event::Paste` is handled by both. On
//! Windows Terminal it never arrives. The escape sequence goes out and the terminal honours it —
//! but crossterm's Windows input path reads `INPUT_RECORD`s from the console API rather than
//! parsing the VT stream, so the `ESC[200~` wrapper is not what reaches the application. The paste
//! arrives as ordinary key events, and every newline in it is an `Enter`.
//!
//! What that looked like in the product: pasting three paragraphs into the conversation **sent
//! three messages**, and pasting into a run window sent three steers. The `[Pasted #1 +340 lines]`
//! chip never appeared on the platform this project develops on.
//!
//! **Every paste test passed throughout**, because they call `App::paste` directly. They assert the
//! handler and never the path — the same shape as a pipe-tested hook, `persona/v1.md` *loaded*
//! versus the persona being in the request body, and `inline_threshold_bytes: 0` with no reader.
//! The rule this project keeps re-learning: *ask what would be true if the mechanism were broken*.
//! Here the answer was "the tests are identical", so they were never evidence about pasting.
//!
//! # What is used instead, and why it is not a timing guess
//!
//! **A paste is already in the input queue; typing is not.** Both event loops read one event and
//! then continue, so anything still queued at that instant arrived faster than the loop can turn
//! over. `event::poll(Duration::ZERO)` asks *"is there already more?"* — a question about the
//! queue, not about the clock. Nothing is measured, so `determinism_guard.rs` and §6.4's
//! no-clock property are untouched.
//!
//! The first draft of this idea *was* a timing guess — "characters arriving less than N ms apart"
//! — and it is exactly the guess this project warns about: it fires on a fast typist, it is
//! invisible in a test, and it needs a clock in a path that deliberately has none.
//!
//! # The threshold, and what it costs to be wrong
//!
//! [`BURST_MIN`] events must arrive in one drain. A human keystroke lands alone, so a burst means
//! the terminal delivered a block. Being wrong in each direction:
//!
//! * **Too low** — a key held down on auto-repeat could collapse into a chip. `Enter` is what makes
//!   this safe: a burst is only treated as a paste when it is large, and auto-repeat of a printable
//!   key produces one character repeated, which a person can see and undo with one `^h`.
//! * **Too high** — a short paste goes in as keystrokes, which is the behaviour that was there
//!   before and is harmless *except* that an embedded newline would send. So newlines dominate the
//!   decision: **any burst carrying a newline is a paste**, regardless of size, because a newline
//!   that a human typed arrives alone.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

/// How many events must arrive together before a burst without a newline is read as a paste.
pub const BURST_MIN: usize = 12;

/// The most events one drain will take, so a pathological stream cannot hold the loop.
const BURST_CAP: usize = 65_536;

/// How long to keep listening after the input queue runs dry, before calling a burst finished.
///
/// # This exists because of a measurement, and the measurement changed the design
///
/// The first version drained only what was *already* queued — `poll(Duration::ZERO)` — on the
/// reasoning that a paste is delivered as a block. `--input-trace` says otherwise. Windows Terminal
/// writes a paste into the console input buffer **in chunks**, and this loop drains faster than it
/// writes:
///
/// ```text
/// drained  1 event(s): "T"
/// drained 48 event(s): "he traced wi"
/// drained  1 event(s): "\""
/// drained  8 event(s): "marlowe "
/// ```
///
/// One paste, dozens of fragments, singletons among them. No fragment is the whole thing, so
/// nothing could ever be represented as one.
///
/// # Why 10 ms is not the timing guess this module refused twice
///
/// The guess that keeps being refused is *"characters arriving less than N ms apart are a paste"* —
/// a rule about **human typing speed**, which fires on a fast typist and needs a clock.
///
/// This is a different question: *"has the terminal finished writing?"* The gap being bridged is a
/// buffer running dry mid-write, which is microseconds of scheduling, not a person's hands. And it
/// is `event::poll`'s own bounded wait — the construct this loop already paces itself with, and
/// the reason `watch.rs` can say *"nothing here measures time"*. No clock is read, so
/// `determinism_guard.rs` and §6.4 are untouched.
///
/// **The cost, named:** an isolated keystroke now waits up to 10 ms before its frame is drawn,
/// because the reader cannot know it was isolated until the wait expires. That is a twelfth of the
/// 120 ms lag fixed earlier in this session and well under the ~50 ms where typing stops feeling
/// immediate — but it is a real cost paid on every key, and it is the thing to re-measure first if
/// the composer ever feels heavy again.
const GRACE_MS: u64 = 10;

/// What one read turned out to be.
pub enum Burst {
    /// A block the terminal delivered. Newlines are real newlines, not `Enter`.
    Paste(String),
    /// Ordinary keys, in order. Usually one.
    Keys(Vec<KeyEvent>),
}

/// Read everything already queued behind `first` and decide what it was.
///
/// **Order is preserved in both arms.** A burst that is not a paste is handed back as the keys it
/// was, so nothing is dropped and nothing is reordered — the failure that would look like a
/// dropped keystroke, which §B13 has a test for.
pub fn read_burst(first: KeyEvent) -> io::Result<Burst> {
    let mut keys = vec![first];
    'burst: loop {
        // Everything already queued, first — the common case costs nothing.
        while keys.len() < BURST_CAP && event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => keys.push(k),
                // Key releases are consumed rather than collected: Windows reports both edges, and
                // a paste is the down edges.
                Event::Key(_) => {}
                // A mouse or resize event ends the burst. It cannot be put back, and a paste does
                // not contain one — so reaching here means this was never a paste.
                _ => break 'burst,
            }
        }
        // **The queue running dry does not mean the terminal has finished.** See `GRACE_MS`.
        if keys.len() >= BURST_CAP || !event::poll(Duration::from_millis(GRACE_MS))? {
            break;
        }
    }

    trace(&keys);

    let text: Option<String> = keys
        .iter()
        .map(|k| match k.code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            KeyCode::Tab => Some('\t'),
            _ => None,
        })
        .collect();

    // A single event is a keystroke, always. Deciding otherwise would make one `Enter` a paste.
    if keys.len() > 1 {
        if let Some(text) = text {
            // **A newline decides it on its own.** A human's `Enter` arrives alone; one that
            // arrived inside a burst came from a block, and treating it as "send" is the defect
            // this module exists for.
            if text.contains('\n') || keys.len() >= BURST_MIN {
                return Ok(Burst::Paste(text));
            }
        }
    }
    Ok(Burst::Keys(keys))
}

/// **A measurement, not a guess.** Set `MARLOWE_INPUT_TRACE=<file>` and every drain appends one
/// line: how many key events arrived together, and the first few characters.
///
/// This exists because the fix above was written from an argument about what a terminal *should*
/// deliver, and the product still did not collapse a paste. A description of a mechanism is not a
/// measurement of its output — the rule this project keeps relearning — so this records what
/// actually arrives, on the machine where it is not working.
fn trace(keys: &[KeyEvent]) {
    let Ok(path) = std::env::var("MARLOWE_INPUT_TRACE") else { return };
    use std::io::Write as _;
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let sample: String = keys
        .iter()
        .take(12)
        .map(|k| match k.code {
            KeyCode::Char(c) => c,
            KeyCode::Enter => '\n',
            _ => '?',
        })
        .collect();
    let _ = writeln!(f, "assembled {} event(s): {sample:?}", keys.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    /// The decision, factored out of the I/O so it can be tested without a terminal — which is
    /// precisely what the paste tests could not do, and why this defect shipped.
    fn classify(keys: &[KeyEvent]) -> Option<String> {
        let text: Option<String> = keys
            .iter()
            .map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                KeyCode::Enter => Some('\n'),
                KeyCode::Tab => Some('\t'),
                _ => None,
            })
            .collect();
        if keys.len() > 1 {
            if let Some(t) = text {
                if t.contains('\n') || keys.len() >= BURST_MIN {
                    return Some(t);
                }
            }
        }
        None
    }

    #[test]
    fn one_keystroke_is_never_a_paste() {
        assert_eq!(classify(&[key('a')]), None);
        // Especially this one. `Enter` is send; a lone one becoming a newline would break sending
        // entirely, which is a worse bug than the one being fixed.
        assert_eq!(classify(&[enter()]), None);
    }

    #[test]
    fn a_burst_carrying_a_newline_is_a_paste_however_short() {
        // The case from the product: three paragraphs pasted into the conversation sent three
        // messages, because each embedded newline was read as `Enter`.
        let keys = vec![key('h'), key('i'), enter(), key('y'), key('o')];
        assert_eq!(classify(&keys).as_deref(), Some("hi\nyo"));
    }

    #[test]
    fn a_short_burst_with_no_newline_stays_keystrokes() {
        // Two fast characters are two characters. Collapsing them into a chip would be worse than
        // the bug: the text is the same either way, and a chip for two letters is nonsense.
        let keys = vec![key('h'), key('i')];
        assert_eq!(classify(&keys), None);
    }

    #[test]
    fn a_long_burst_with_no_newline_is_a_paste() {
        // A pasted URL or path has no newline in it and is still a paste.
        let keys: Vec<KeyEvent> = "https://example.com/a/very/long/path".chars().map(key).collect();
        assert!(keys.len() >= BURST_MIN);
        assert!(classify(&keys).is_some());
    }

    #[test]
    fn a_burst_containing_a_non_text_key_is_not_a_paste() {
        // A held arrow key, or a chord mixed into fast typing. A paste is text; anything else in
        // the batch means this came from a keyboard and must be replayed as keys.
        let mut keys: Vec<KeyEvent> = "hello world".chars().map(key).collect();
        keys.push(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(classify(&keys), None);
    }
}
