//! §B10: **getting a conversation out of the application.**
//!
//! > A conversation the user cannot get out of the application is a conversation trapped in it.
//!
//! Mouse capture took the terminal's own selection away, so the replacement has to be real rather
//! than "use the keyboard". Three things carry that, and they are deliberately different tools:
//!
//! | | what it copies | why it exists |
//! |---|---|---|
//! | `Shift`-drag | whatever the terminal selects | the escape hatch, verified working under capture |
//! | `y` | the focused turn, or a tool call's **full expanded output** | the common case |
//! | `Y` | the whole transcript as markdown | the one worth pasting somewhere |
//!
//! # These payloads are built from the model, never from the screen
//!
//! Copying rendered cells is what native selection already does, and the measured result of doing
//! it under this layout is:
//!
//! ```text
//!              +3 −0 ││ ┌Spend───────────────────────────────────────────┐
//! ```
//!
//! Borders, the scrollbar column, and inspector content that happens to share a row. That is not a
//! deficiency of Windows Terminal — it is what cell-rectangle selection *is*, and no amount of care
//! in the emulator can fix it, because the emulator cannot know that column 62 belongs to a
//! different region than column 61. Building from `Entry` and `ToolCall` sidesteps the whole class:
//! there is no border to strip because a border was never in the data.
//!
//! # OSC 52, and the failure mode it has
//!
//! The clipboard write is OSC 52 — the terminal's own clipboard protocol. It needs no dependency,
//! and it works over SSH, which a harness that will grow remote sessions needs. Verified against
//! Windows Terminal 1.24 with a canary before being chosen.
//!
//! **It is write-only and unacknowledged.** A terminal that does not implement it (or has it
//! disabled, as xterm does without `allowWindowOps`) silently discards the sequence, and this
//! module cannot tell. That is precisely the unobservable-mismatch pattern CLAUDE.md warns about,
//! so it is handled the way the braille glyphs are: **the app says what it did**, in the status
//! band, with the byte count — and `marlowe doctor` carries a canary the user confirms by paste.
//! An honest "copied 412 characters" next to an empty clipboard is a bug report; a silent no-op is
//! a mystery.

use marlowe_view::{Entry, SessionView, ToolCall};
use marlowe_view::turn::ToolLineState;

/// Wrap text in an OSC 52 clipboard-write sequence.
///
/// `52;c;` targets the system clipboard (`c`), not a selection buffer. BEL-terminated for the same
/// reason as the OSC 10/11 ground sequences: every emulator accepts it, while ST is stricter and
/// less widely handled.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Base64, written out rather than pulled in.
///
/// A dependency for forty lines of table lookup would be the larger cost, and this has no
/// configuration surface to get wrong: no line wrapping (OSC 52 payloads are a single run), no
/// URL-safe alphabet, always padded.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A tool call as text, **always fully expanded**.
///
/// §B6 collapses six reads into `⋯ read  6 files` and hides a failure's detail behind an expansion.
/// That is right on screen, where the point is to keep one turn readable. It is wrong in a
/// clipboard: a user copying a failed tool call wants the error, and copying the one-line summary
/// would hand them the thing they were trying to look past. What is on the clipboard is therefore
/// not what is on the screen, and that is the intended behaviour rather than a divergence.
pub fn tool_call_text(t: &ToolCall) -> String {
    let mut s = format!("{} {}", t.verb, t.target);
    for target in &t.collapsed {
        s.push_str(&format!("\n{} {}", t.verb, target));
    }
    match &t.state {
        ToolLineState::Running { elapsed_ms } => {
            s.push_str(&format!("\n  running, {elapsed_ms} ms so far"));
        }
        ToolLineState::Ok(r) | ToolLineState::Failed(r) => {
            let outcome = if t.is_failure() { "failed" } else { "ok" };
            // Rendered upstream (the daemon's fold) or built from typed metrics here. `y` copied
            // `ok` alone on every wire-fed call before `summary_line` existed.
            let summary = match &t.summary_line {
                Some(line) => line.clone(),
                None => r.metrics.iter().map(|m| m.render()).collect::<Vec<_>>().join(" · "),
            };
            if summary.is_empty() {
                s.push_str(&format!("\n  {outcome}"));
            } else {
                s.push_str(&format!("\n  {outcome} · {summary}"));
            }
            if let Some(d) = &r.detail {
                // **The clipboard is an egress, and `detail` is tool output now.** This copied it
                // verbatim, which was correct while it held a harness constant; 32 KB of `bash`
                // stdout puts an unterminated ESC sequence on the clipboard, and it fires in
                // whatever terminal the user pastes into — a terminal Marlowe does not draw.
                //
                // `sanitize_prose`, not `mark_reserved`: a clipboard is not a Marlowe frame and
                // has no chrome to forge, and a pasted box-drawing character is what the user
                // asked to copy.
                let d = marlowe_contract::text::sanitize_prose(d);
                for line in d.lines() {
                    s.push_str(&format!("\n  {line}"));
                }
            }
        }
    }
    s
}

/// One transcript entry as plain text, or `None` for entries with nothing to copy.
pub fn entry_text(e: &Entry) -> Option<String> {
    match e {
        Entry::User(t) => Some(t.clone()),
        Entry::Said(marlowe_view::Speech::Model(t)) => Some(t.clone()),
        // Harness speech is not conversation. `y` on a `/help` listing copying the listing would
        // be a copy of the tool's output masquerading as a turn.
        Entry::Said(marlowe_view::Speech::Harness(_)) => None,
        // Reasoning is not what Marlowe said. Copying a turn must not hand the user a chain of
        // thought they collapsed precisely because they did not want to read it.
        Entry::Reasoning { .. } => None,
        Entry::Tools(calls) if calls.is_empty() => None,
        Entry::Tools(calls) => Some(
            calls
                .iter()
                .map(tool_call_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        // A compaction marker is chrome about the conversation, not part of it.
        Entry::Compacted { .. } => None,
    }
}

/// The whole transcript as markdown — what `Y` produces.
///
/// Speakers become headings rather than being flattened, because the thing that makes a pasted
/// transcript useful is knowing who said what. Tool calls become fenced blocks: they are output,
/// and markdown that renders a diff summary as prose is markdown nobody can read.
pub fn transcript_markdown(view: &SessionView) -> String {
    let mut out = String::new();
    for e in &view.transcript {
        match e {
            Entry::User(t) => {
                out.push_str(&format!("**You:** {t}\n\n"));
            }
            Entry::Said(marlowe_view::Speech::Model(t)) => {
                out.push_str(&format!("**Marlowe:** {t}\n\n"));
            }
            // §B10's `Y` copies the CONVERSATION. Harness lines are the tool answering and are
            // deliberately absent from the markdown, exactly as a shell's output is absent from a
            // transcript of what two people said.
            Entry::Said(marlowe_view::Speech::Harness(_)) => {}
            // Absent from a `Y` markdown transcript for the same reason.
            Entry::Reasoning { .. } => {}
            Entry::Tools(calls) if !calls.is_empty() => {
                out.push_str("```\n");
                for c in calls {
                    out.push_str(&tool_call_text(c));
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            Entry::Tools(_) => {}
            Entry::Compacted { turns } => {
                out.push_str(&format!("_— compacted · {turns} turns —_\n\n"));
            }
        }
    }
    // One trailing newline, not three. A paste should not arrive with a hole under it.
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        // RFC 4648 §10, including both padding cases — the two the hand-rolled encoder can get
        // wrong and the only ones a canary paste would not obviously reveal.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_is_well_formed_and_carries_utf8() {
        let s = osc52("marlowe");
        assert!(s.starts_with("\x1b]52;c;"), "OSC 52 introducer");
        assert!(s.ends_with('\x07'), "BEL terminator");
        // Non-ASCII has to survive, or the braille meter and the em-dashes in the persona's prose
        // arrive as mojibake on the clipboard.
        assert_eq!(base64("⋯".as_bytes()), "4ouv");
    }

    #[test]
    fn a_failed_tool_call_copies_its_detail_not_its_summary() {
        // The whole reason copy expands: the user copying a failure wants the error.
        let t = ToolCall::failed(
            1,
            "run",
            "cargo test",
            vec![marlowe_view::turn::Metric::Exit { code: 1 }],
            "thread 'a' panicked at src/lib.rs:42",
        );
        let text = tool_call_text(&t);
        assert!(text.contains("cargo test"));
        assert!(text.contains("failed"));
        assert!(
            text.contains("panicked at src/lib.rs:42"),
            "the detail is the point of copying a failure: {text}"
        );
    }

    #[test]
    fn a_collapsed_group_copies_every_target_it_hid() {
        // §B6 shows `⋯ read 6 files`. Six filenames is what the user is actually after.
        let mut t = ToolCall::ok(1, "read", "a.rs", vec![]);
        t.collapsed = vec!["b.rs".into(), "c.rs".into()];
        let text = tool_call_text(&t);
        for f in ["a.rs", "b.rs", "c.rs"] {
            assert!(text.contains(f), "{f} was collapsed away in the copy: {text}");
        }
    }

    #[test]
    fn copied_text_carries_no_frame() {
        // The measured failure of native selection, asserted against: no border glyphs, no
        // scrollbar column, nothing from a region that merely shared a row.
        let s = marlowe_stub::Session::new();
        let md = transcript_markdown(s.view());
        for glyph in ['│', '─', '┌', '┐', '└', '┘', '║', '█'] {
            assert!(
                !md.contains(glyph),
                "{glyph:?} reached the clipboard; the payload is being built from the screen"
            );
        }
        assert!(!md.is_empty());
        assert!(md.ends_with('\n') && !md.ends_with("\n\n"));
    }
}
