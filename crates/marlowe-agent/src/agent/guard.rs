//! Loop guards (design 2.10 B3): the ways a model, local ones above all, stalls inside a turn.
//!
//! - [`RepetitionWatch`]: a streamed reply (or its reasoning) stuck repeating itself. The loop
//!   cuts the call, keeps one copy ([`trim_repetition`]) and tells the model ([`REPEATING`]).
//! - [`LoopGuard`] middleware: the same failure three calls running gets a note on its result;
//!   the same calls with the same arguments three steps running get a reminder; and, once per
//!   turn each, ending the turn right after announcing a tool call, or right after a failed call
//!   without trying again, gets a nudge.
//!
//! Test cases follow FlashAgent's (`crates/core/src/repetition.rs`, `loop_.rs`, MIT); the code
//! is Marlowe's.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use marlowe_model::{Message, Part, Role};

use crate::convo::Conversation;
use crate::middleware::{FinalFlow, StepInfo, ToolMiddleware};
use crate::tools::{CallCtx, ToolCallReq, ToolOutcome};

/// Text shorter than this is never judged.
const MIN_TEXT: usize = 24;
/// Shorter lines (braces, list markers) repeat legitimately.
const MIN_LINE: usize = 6;
/// A sentence this long that comes back three times is a loop.
const MIN_CLAUSE: usize = 25;
/// Finished prose lines remembered for the line checks.
const RECENT_LINES: usize = 6;

/// Told to the model after its reply was cut for repeating itself.
pub const REPEATING: &str = "<system-reminder>\nYour last reply started repeating itself and was cut off by the \
harness after its first copy. Do not repeat it. Continue from where the useful part ended: make the next tool \
call, or give the final answer.\n</system-reminder>";

/// Watches one streamed text (reply or reasoning) for degenerate repetition. Feed it the whole
/// text so far after every delta; each byte is examined a bounded number of times.
#[derive(Debug, Default)]
pub struct RepetitionWatch {
    /// Bytes of finished lines already folded in.
    done: usize,
    /// Bytes already looked at for sentence and line ends.
    scanned: usize,
    in_fence: bool,
    prose_lines: usize,
    recent: Vec<String>,
    /// Sentences of finished prose, counted at most once per line run.
    clauses: HashMap<String, u32>,
    clause_lengths: HashSet<usize>,
    thrice: bool,
    /// Byte offset in the current line where its next sentence starts, and the last sentence
    /// counted in it (an immediate repeat inside one line counts once).
    line_clause_start: usize,
    line_last: Option<String>,
    line_is_prose: Option<bool>,
}

impl RepetitionWatch {
    /// True once `text` (the previous text plus a delta) is looping.
    pub fn is_looping(&mut self, text: &str) -> bool {
        if text.len() < self.scanned {
            *self = Self::default();
        }
        self.fold(text);
        if text.len() < MIN_TEXT {
            return false;
        }
        let partial = text[self.done..].trim();
        let partial_prose = !self.in_fence && !partial.is_empty() && !partial.starts_with("```");
        let mut lines: Vec<&str> = self.recent.iter().map(String::as_str).collect();
        if partial_prose {
            lines.push(partial);
        }
        if same_line_thrice(&lines) || alternating_pair(&lines) || tail_repeats(text.as_bytes()) {
            return true;
        }
        let prose = self.prose_lines + usize::from(partial_prose);
        prose >= 4 && (self.thrice || (partial_prose && self.open_clause_is_third(text)))
    }

    /// Folds every finished sentence and line of `text` past what was seen.
    fn fold(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut i = self.scanned.max(self.done + self.line_clause_start);
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    self.end_clause(text, self.done + self.line_clause_start, i);
                    self.end_line(text, i);
                }
                b'.' | b'!' | b'?' | b';' => {
                    self.end_clause(text, self.done + self.line_clause_start, i);
                    self.line_clause_start = i + 1 - self.done;
                }
                _ => {}
            }
            i += 1;
        }
        self.scanned = bytes.len();
    }

    fn line_prose(&mut self, text: &str) -> bool {
        let (fence, start) = (self.in_fence, self.done);
        *self.line_is_prose.get_or_insert_with(|| !fence && !text[start..].trim_start().starts_with("```"))
    }

    fn end_clause(&mut self, text: &str, start: usize, end: usize) {
        let clause = text[start..end].trim();
        if clause.len() < MIN_CLAUSE || !self.line_prose(text) || self.line_last.as_deref() == Some(clause) {
            return;
        }
        self.line_last = Some(clause.to_string());
        let n = self.clauses.entry(clause.to_string()).or_insert(0);
        *n += 1;
        self.thrice |= *n >= 3;
        self.clause_lengths.insert(clause.len());
    }

    fn end_line(&mut self, text: &str, end: usize) {
        let line = text[self.done..end].trim();
        if line.starts_with("```") {
            self.in_fence = !self.in_fence;
        } else if !self.in_fence && !line.is_empty() {
            self.prose_lines += 1;
            if self.recent.len() == RECENT_LINES {
                self.recent.remove(0);
            }
            self.recent.push(line.to_string());
        }
        self.done = end + 1;
        self.line_clause_start = 0;
        self.line_last = None;
        self.line_is_prose = None;
    }

    /// The sentence still being written would be the third copy.
    fn open_clause_is_third(&self, text: &str) -> bool {
        let open = text[self.done + self.line_clause_start..].trim();
        open.len() >= MIN_CLAUSE
            && self.clause_lengths.contains(&open.len())
            && self.line_last.as_deref() != Some(open)
            && self.clauses.get(open).is_some_and(|n| n + 1 >= 3)
    }
}

fn same_line_thrice(lines: &[&str]) -> bool {
    let n = lines.len();
    n >= 3 && lines[n - 1].len() >= MIN_LINE && lines[n - 2] == lines[n - 1] && lines[n - 3] == lines[n - 1]
}

/// A B A B A B.
fn alternating_pair(lines: &[&str]) -> bool {
    let n = lines.len();
    if n < 6 {
        return false;
    }
    let (a, b) = (lines[n - 1], lines[n - 2]);
    (a.len() >= MIN_LINE || b.len() >= MIN_LINE)
        && (3..=6).step_by(2).all(|k| lines[n - k] == a)
        && (4..=6).step_by(2).all(|k| lines[n - k] == b)
}

/// The text ends in four copies of one 8-80 byte chunk (48 bytes at least). A chunk of one
/// byte, or of table and rule characters, is decoration.
fn tail_repeats(bytes: &[u8]) -> bool {
    let len = bytes.len();
    (8..=80usize).any(|w| {
        if len < w * 4 || w * 4 < 48 {
            return false;
        }
        let last = &bytes[len - w..];
        (2..=4).all(|k| &bytes[len - w * k..len - w * (k - 1)] == last)
            && !last.iter().all(|&b| b == last[0])
            && !last.iter().all(|b| b"|-:=*#_~. \n".contains(b))
    })
}

/// Cuts a looping text back to one copy of what it repeated: a run of identical trailing lines
/// keeps the first, a repeated trailing chunk keeps one.
pub fn trim_repetition(text: &str) -> String {
    let body = text.trim_end();
    let lines: Vec<&str> = body.lines().collect();
    if let Some(last) = lines.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()) {
        if last.len() >= MIN_LINE {
            let mut first = lines.len();
            let mut copies = 0;
            for (i, l) in lines.iter().enumerate().rev() {
                if l.trim() == last {
                    copies += 1;
                    first = i;
                } else if !l.trim().is_empty() {
                    break;
                }
            }
            if copies > 1 {
                return lines[..=first].join("\n");
            }
        }
    }
    let bytes = body.as_bytes();
    let len = bytes.len();
    for w in 8..=80usize {
        if len >= w * 3 {
            let last = &bytes[len - w..];
            if (2..=3).all(|k| &bytes[len - w * k..len - w * (k - 1)] == last) && !last.iter().all(|&b| b == last[0]) {
                let mut cut = len - w * 2;
                while !body.is_char_boundary(cut) {
                    cut -= 1;
                }
                return body[..cut].to_string();
            }
        }
    }
    body.to_string()
}

// ---------------------------------------------------------------- the middleware

/// The same failing result this many calls in a row gets a note.
pub const REPEATED_FAILURE_AT: u32 = 3;
/// The same batch of calls this many steps in a row gets a reminder.
pub const REPEATED_CALLS_AT: u32 = 3;

/// Sent once per turn when the model said what it would do next and ended the turn instead.
pub const PROMISE_NUDGE: &str = "<system-reminder>\nYou said what you would do next but ended your turn without \
doing it. Make that tool call now.\n</system-reminder>";

/// Sent once per turn when the model ended right after a failed call without trying again.
pub const ERROR_NUDGE: &str = "<system-reminder>\nYour last tool call failed and you ended the turn without trying \
again. If the error shows what to change (a path, a name, an argument, the code), make the corrected call now. \
Otherwise say in one sentence what blocks you.\n</system-reminder>";

fn repeated_failure_note(times: u32) -> String {
    format!(
        "\n\n[harness: this is the same failure {times} calls in a row. Repeating the call will not change it. Read \
         the target again to see its current state, or take a different approach; if nothing works, say what blocks you.]"
    )
}

fn repeated_calls_note(times: u32) -> String {
    format!(
        "<system-reminder>\nYou have made the same tool calls with the same arguments {times} steps in a row, and the \
         results did not change. Do not repeat them: use what they returned, try something different, or give your \
         answer.\n</system-reminder>"
    )
}

/// The guard's per-session state in `Conversation::ext`.
#[derive(Debug, Clone, Default)]
pub struct GuardState {
    /// `(tool + first error line, consecutive count)`.
    failure: Option<(String, u32)>,
    /// The last steps' call signatures and how many steps in a row it repeated.
    calls: Option<(String, u32)>,
    /// A call that ran in the current batch failed (denied and blocked calls never run, so a
    /// denial is never taken for a failure to retry).
    batch_failed: bool,
    /// Whether the last step's batch had a failed call.
    last_batch_failed: bool,
    /// `(turn, promise nudged, error nudged)`.
    nudged: (u32, bool, bool),
}

/// The guard middleware (see the module docs).
#[derive(Debug, Default)]
pub struct LoopGuard;

impl LoopGuard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// The last assistant message's calls as one canonical string.
fn last_calls_signature(messages: &[Message]) -> Option<String> {
    let m = messages.iter().rev().find(|m| m.role == Role::Assistant)?;
    let sig: Vec<String> = m
        .content
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(format!("{}{}", c.name, c.args)),
            _ => None,
        })
        .collect();
    (!sig.is_empty()).then(|| sig.join("\u{1f}"))
}

/// Whether `text` ends by announcing an action the model then did not take ("Let me run the
/// tests.", "Now I'll fix the import:"). A question, or words to the user, is not one.
pub fn announces_action(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let paragraph = text.rsplit("\n\n").next().unwrap_or(text);
    if paragraph.contains('?') {
        return false;
    }
    let body = text.trim_end_matches(['.', ':', '\u{2026}', '!']);
    let start = [". ", "! ", "\n"].iter().filter_map(|s| body.rfind(s).map(|i| i + s.len())).max().unwrap_or(0);
    let last = body[start..].trim().trim_start_matches(['*', '#', '-', '>', ' ']).to_lowercase();
    const TO_THE_USER: [&str; 9] =
        ["let me know", "you ", "you.", "your ", "happy to", "feedback", "if you", "once you", "when you"];
    if TO_THE_USER.iter().any(|c| last.contains(c)) || last.ends_with("you") {
        return false;
    }
    const ACTING: [&str; 14] = [
        "i'll ", "i will ", "let me ", "let's ", "i'm going to ", "i am going to ", "now i'll ", "next i'll ",
        "next, i'll ", "first, i'll ", "first i'll ", "now let me ", "now, let me ", "i need to ",
    ];
    ACTING.iter().any(|o| last.starts_with(o))
}

impl ToolMiddleware for LoopGuard {
    fn name(&self) -> &str {
        "loop-guard"
    }

    fn after_call(&self, call: &ToolCallReq, cx: &CallCtx, outcome: &mut ToolOutcome) {
        cx.scope.with_ext(|e| {
            let st = e.get_or_default::<GuardState>();
            st.batch_failed |= outcome.is_error;
            if !outcome.is_error {
                st.failure = None;
                return;
            }
            let first: String = outcome.content.lines().next().unwrap_or("").chars().take(120).collect();
            let key = format!("{}: {first}", call.name);
            let n = match st.failure.take() {
                Some((k, n)) if k == key => n + 1,
                _ => 1,
            };
            if n >= REPEATED_FAILURE_AT && (n - REPEATED_FAILURE_AT) % 2 == 0 {
                outcome.content.push_str(&repeated_failure_note(n));
            }
            st.failure = Some((key, n));
        });
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        let sig = last_calls_signature(&convo.messages);
        let st = convo.ext.get_or_default::<GuardState>();
        st.last_batch_failed = std::mem::take(&mut st.batch_failed);
        let Some(sig) = sig else {
            st.calls = None;
            return Vec::new();
        };
        let n = match st.calls.take() {
            Some((s, n)) if s == sig => n + 1,
            _ => 1,
        };
        st.calls = Some((sig, n));
        if n == REPEATED_CALLS_AT {
            return vec![Message::user(repeated_calls_note(n))];
        }
        Vec::new()
    }

    fn before_final(&self, info: StepInfo, convo: &mut Conversation, text: &str) -> FinalFlow {
        let st = convo.ext.get_or_default::<GuardState>();
        if st.nudged.0 != info.turn {
            st.nudged = (info.turn, false, false);
            // A failure from an earlier turn is not this turn's.
            if info.step <= 1 {
                st.last_batch_failed = false;
            }
        }
        if !st.nudged.1 && announces_action(text) {
            st.nudged.1 = true;
            return FinalFlow::Continue(PROMISE_NUDGE.into());
        }
        let question = text.trim().rsplit("\n\n").next().unwrap_or("").contains('?');
        if !st.nudged.2 && st.last_batch_failed && info.step > 1 && !question {
            st.nudged.2 = true;
            st.last_batch_failed = false;
            return FinalFlow::Continue(ERROR_NUDGE.into());
        }
        FinalFlow::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn looping(text: &str) -> bool {
        RepetitionWatch::default().is_looping(text)
    }

    /// Fed in deltas of several sizes, the watch reaches the same verdict on the whole text.
    fn looping_streamed(text: &str) -> bool {
        let mut verdicts = Vec::new();
        for step in [1, 3, 7, 40, usize::MAX] {
            let mut w = RepetitionWatch::default();
            let mut end = 0;
            let mut hit = false;
            while end < text.len() {
                end = end.saturating_add(step).min(text.len());
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                hit |= w.is_looping(&text[..end]);
            }
            verdicts.push(hit);
        }
        assert!(verdicts.iter().all(|v| *v == verdicts[0]), "{text:?}: {verdicts:?}");
        verdicts[0]
    }

    // FlashAgent (MIT, 0b08afcd) repetition.rs cases.
    const CAUGHT: &[&str] = &[
        "*Wait, I'll call list_dir.*\n*Wait, I'll call list_dir.*\n*Wait, I'll call list_dir.*\n",
        "Check the input first.\nThen run the tests.\nCheck the input first.\nThen run the tests.\nCheck the input first.\nThen run the tests.\n",
        "line one\r\nline two is here\r\nline two is here\r\nline two is here\r\n",
        "  indented and repeated line  \n\tindented and repeated line\nindented and repeated line   \n",
        "abcdefgh1234abcdefgh1234abcdefgh1234abcdefgh1234abcdefgh1234",
        "the clause that is long enough to count here; another clause long enough to count too; the clause that is long enough to count here; another clause long enough to count too; the clause that is long enough to count here\nsecond\nthird\nfourth",
        "Response construction:\nTurn it over. Then the sealed top becomes the bottom, and the open bottom becomes the top.\nWait, if I am an AI assistant for coding, should I even answer riddles?\nFinal decision:\nTurn it over. Then the sealed top becomes the bottom, and the open bottom becomes the top.\nWait, I don't need to translate my thought process into Russian.\n\"Turn it over. Then the sealed top becomes the bottom, and the open bottom becomes the top.\"\nWait, I'll check if there are any other interpretations.",
    ];

    const NOT_CAUGHT: &[&str] = &[
        "First line of answer.\nSecond line with different content.\nThird line.",
        "Two examples:\n```python\nif __name__ == \"__main__\":\n    main()\n```\nand\n```python\nif __name__ == \"__main__\":\n    run()\n```\nBoth work.",
        "```rust\n#[derive(Debug, Clone, PartialEq)]\nstruct A;\n#[derive(Debug, Clone, PartialEq)]\nstruct B;\n```",
        "```html\n<div><div><div>\n</div>\n</div>\n</div>\n```",
        "| a | b | c | d | e | f |\n|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 |",
        "The configuration file is read at startup\nThe configuration file is read at startup, then cached.\nDone.\nOk.",
        "```\nsame line again and again\nsame line again and again\nsame line again and again\n```\nprose after the fence ends here.",
    ];

    #[test]
    fn every_kind_of_loop_is_caught_and_code_is_left_alone() {
        for t in CAUGHT {
            assert!(looping(t), "missed: {t:?}");
            assert!(looping_streamed(t), "missed when streamed: {t:?}");
        }
        for t in NOT_CAUGHT {
            assert!(!looping(t), "false alarm: {t:?}");
            assert!(!looping_streamed(t), "false alarm when streamed: {t:?}");
        }
    }

    #[test]
    fn a_long_varied_reply_is_watched_in_linear_time() {
        let mut text = String::new();
        let mut w = RepetitionWatch::default();
        let started = std::time::Instant::now();
        for i in 0..5_000 {
            let line = format!("Step {i} checks another part of the program before moving on to the next one.\n");
            for piece in line.split_inclusive(' ') {
                text.push_str(piece);
                assert!(!w.is_looping(&text), "a varied reply was taken for a loop at line {i}");
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
    }

    #[test]
    fn trimming_keeps_one_copy_and_never_splits_a_character() {
        let t = "Here is the plan:\n*Wait, I'll call list_dir.*\n*Wait, I'll call list_dir.*\n*Wait, I'll call list_dir.*";
        assert_eq!(trim_repetition(t), "Here is the plan:\n*Wait, I'll call list_dir.*");
        let spaced = "Plan:\nI'll check the file now.\n\nI'll check the file now.\nI'll check the file now.\n";
        assert_eq!(trim_repetition(spaced), "Plan:\nI'll check the file now.");
        let mut cyr = format!("Ответ: {}", "повторяю снова ".repeat(8));
        cyr.push('ё');
        let _ = trim_repetition(&cyr);
        let _ = trim_repetition(&format!("x{}", "ёжик ёлка ".repeat(10)));
    }

    #[test]
    fn announcements_are_told_from_questions_and_closings() {
        assert!(announces_action("The parser is in src/parse.rs. Let me run the tests."));
        assert!(announces_action("Found it.\nNow I'll fix the import:"));
        assert!(!announces_action("Done. Let me know if you want more."));
        assert!(!announces_action("Should I run the tests? I'll wait."));
        assert!(!announces_action("All tests pass."));
    }
}
