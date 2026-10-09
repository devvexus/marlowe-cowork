//! Log tails: read appended bytes only, classify lines, keep the last meaningful line.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use marlowe_proto::Severity;

/// What a log line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// `done`, `finished`, `complete`, `saved model`, ...
    Completion,
    /// `error`, `exception`, `failed`.
    Error,
    /// `Traceback (most recent call last)`.
    Traceback,
    /// NaN or inf in a numeric context.
    NonFinite,
    /// CUDA / CPU out of memory.
    Oom,
    /// `Killed` (OOM killer or a signal).
    Killed,
    /// Ordinary progress worth keeping as the last line.
    Progress,
    /// Blank, a progress-bar redraw, or noise.
    Other,
}

impl LineKind {
    /// The notice severity this kind raises, if any.
    pub fn severity(self) -> Option<Severity> {
        match self {
            LineKind::Completion => Some(Severity::Good),
            LineKind::Error | LineKind::Traceback => Some(Severity::Attention),
            LineKind::NonFinite | LineKind::Oom | LineKind::Killed => Some(Severity::Alarm),
            LineKind::Progress | LineKind::Other => None,
        }
    }

    /// Worth showing as a run's last line.
    pub fn is_meaningful(self) -> bool {
        !matches!(self, LineKind::Other)
    }

    /// Ends a run badly when it is the last line.
    pub fn is_failure(self) -> bool {
        matches!(self, LineKind::Error | LineKind::Traceback | LineKind::NonFinite | LineKind::Oom | LineKind::Killed)
    }

    /// Notice kind string.
    pub fn as_str(self) -> &'static str {
        match self {
            LineKind::Completion => "completion",
            LineKind::Error => "error",
            LineKind::Traceback => "traceback",
            LineKind::NonFinite => "nan",
            LineKind::Oom => "oom",
            LineKind::Killed => "killed",
            LineKind::Progress => "progress",
            LineKind::Other => "other",
        }
    }
}

/// Classifies one line. The most specific, most severe kinds are checked first, and Python
/// warnings (`FutureWarning: ...`) never count as errors.
pub fn classify_line(line: &str) -> LineKind {
    let t = line.trim_end();
    if t.trim().is_empty() {
        return LineKind::Other;
    }
    let lower = t.to_ascii_lowercase();
    if t.contains("Traceback (most recent call last)") {
        return LineKind::Traceback;
    }
    if lower.contains("out of memory") || t.contains("OutOfMemoryError") || has_word(t, "OOM") {
        return LineKind::Oom;
    }
    if t.trim_start().starts_with("Killed") || lower.contains("killed by signal") || t.contains("SIGKILL") {
        return LineKind::Killed;
    }
    if lower.contains("warning:") || lower.contains("warnings.warn") {
        return LineKind::Other;
    }
    if has_non_finite(t) {
        return LineKind::NonFinite;
    }
    if is_error(t, &lower) {
        return LineKind::Error;
    }
    // Indented continuation lines (warning sources, stack frames) and progress-bar redraws.
    if line.starts_with(' ') || line.starts_with('\t') || t.contains('\r') || t.contains("it/s]") || t.contains("s/it]") || t.contains('█') {
        return LineKind::Other;
    }
    if is_completion(&lower) {
        return LineKind::Completion;
    }
    LineKind::Progress
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == word)
}

/// A `nan`/`inf` token standing as a value: `loss nan`, `loss=NaN`, `'loss': inf`.
fn has_non_finite(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '+' || c == '.'))
        .map(|w| w.trim_matches('.').trim_start_matches(['-', '+']).to_ascii_lowercase())
        .any(|w| matches!(w.as_str(), "nan" | "inf" | "infinity"))
}

fn is_error(t: &str, lower: &str) -> bool {
    // Python exception names: `ValueError: ...`, `torch.cuda.SomethingError`, `KeyError`.
    let exception = t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')).any(|w| {
        let last = w.rsplit('.').next().unwrap_or(w);
        last.len() > 5
            && (last.ends_with("Error") || last.ends_with("Exception"))
            && last.starts_with(|c: char| c.is_ascii_uppercase())
    });
    let trimmed = lower.trim_start();
    exception
        || trimmed.starts_with("error")
        || trimmed.starts_with("fatal:")
        || lower.contains("error:")
        || has_word(t, "ERROR")
        || has_word(t, "FAILED")
        || has_word(t, "ALERT")
        || lower.contains(" is dead")
        || lower.contains("segmentation fault")
}

fn is_completion(lower: &str) -> bool {
    // `not finished`, `done, 3 checks failed`, `completed with errors` are not completions.
    if is_negated(lower) || soft_failure(lower).is_some() {
        return false;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| matches!(w, "finished" | "done" | "complete" | "completed" | "succeeded"))
        || lower.contains("saved model")
        || lower.contains("model saved")
}

fn words(lower: &str) -> Vec<&str> {
    lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '\'')).filter(|w| !w.is_empty()).collect()
}

/// `not written`, `couldn't save`, `unable to`, `never finished`.
fn is_negated(lower: &str) -> bool {
    words(lower).iter().any(|w| matches!(*w, "not" | "never" | "unable" | "cannot" | "incomplete" | "unfinished") || w.ends_with("n't"))
}

/// A failure said in lowercase words: `check failed`, `3 tests failed`, `with errors`, `aborted`.
/// `0 failed` / `no errors` are not failures. The short reason the brief may say.
fn soft_failure(lower: &str) -> Option<String> {
    let w = words(lower);
    let bad = w.iter().enumerate().find(|(i, x)| {
        let hit = matches!(**x, "failed" | "fails" | "failure" | "failures" | "errors" | "aborted" | "aborting" | "crashed");
        let zero = i.checked_sub(1).and_then(|j| w.get(j)).is_some_and(|p| matches!(*p, "0" | "no" | "zero" | "without"));
        hit && !zero
    })?;
    let checked = w.iter().any(|x| matches!(*x, "check" | "checks" | "verify" | "verification" | "test" | "tests" | "eval" | "validation"));
    Some(match *bad.1 {
        "failed" | "fails" | "failure" | "failures" if checked => "its check failed".into(),
        "aborted" | "aborting" => "aborted".into(),
        "crashed" => "crashed".into(),
        "errors" => "errors".into(),
        _ => "it reported a failure".into(),
    })
}

/// How a process's last meaningful line says it ended. Judged only once the process is gone:
/// `=== stage X finished` in a live run is progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    Success,
    /// A short reason: `out of memory`, `RuntimeError`, `exit code 137`.
    Failure(String),
    Unknown,
}

/// The ending a last line states. Beyond [`classify_line`]'s completions, an artifact written,
/// saved, exported or shipped, a passed check, and llama.cpp's closing `quant size ... BPW` count
/// as success; a non-zero exit code counts as failure. A negated line (`not saved`) is not success.
pub fn ending(line: &str) -> Ending {
    if let Some(code) = exit_code(line) {
        return if code == 0 { Ending::Success } else { Ending::Failure(format!("exit code {code}")) };
    }
    let lower = line.to_ascii_lowercase();
    match classify_line(line) {
        LineKind::Completion => Ending::Success,
        LineKind::Error => Ending::Failure(exception_name(line).or_else(|| soft_failure(&lower)).unwrap_or_else(|| "an error".into())),
        LineKind::Progress | LineKind::Other if soft_failure(&lower).is_some() => Ending::Failure(soft_failure(&lower).unwrap_or_default()),
        LineKind::Oom => Ending::Failure("out of memory".into()),
        LineKind::Killed => Ending::Failure("killed".into()),
        LineKind::NonFinite => Ending::Failure("NaN/inf".into()),
        LineKind::Traceback => Ending::Failure("a Python traceback".into()),
        LineKind::Progress if is_success(&lower) => Ending::Success,
        LineKind::Progress | LineKind::Other => Ending::Unknown,
    }
}

fn is_success(lower: &str) -> bool {
    let negated = is_negated(lower) || soft_failure(lower).is_some();
    let word = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| matches!(w, "written" | "wrote" | "saved" | "exported" | "shipped" | "succeeded" | "success"));
    let phrase = ["check ok", "checks ok", "check passed", "checks passed"].iter().any(|p| lower.contains(p))
        || (lower.contains("quant size") && lower.contains("bpw"));
    !negated && (word || phrase)
}

/// `exit code 1`, `exited with code 137`, `exit status 2`, `returned non-zero exit status 1`.
fn exit_code(line: &str) -> Option<i64> {
    let lower = line.to_ascii_lowercase();
    let at = ["exit code", "exit status", "exited with code"].iter().filter_map(|k| lower.find(k).map(|i| i + k.len())).min()?;
    let digits: String = lower[at..].trim_start().trim_start_matches([':', '=']).trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

/// The last component of a Python exception name on the line: `torch.cuda.FooError` -> `FooError`.
fn exception_name(line: &str) -> Option<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')).find_map(|w| {
        let last = w.rsplit('.').next().unwrap_or(w);
        let named = last.len() > 5 && (last.ends_with("Error") || last.ends_with("Exception")) && last.starts_with(|c: char| c.is_ascii_uppercase());
        named.then(|| last.to_string())
    })
}

/// A `[HH:MM:SS]` (or `[YYYY-MM-DD HH:MM:SS...]`, or a leading `HH:MM:SS`) timestamp, as seconds
/// after midnight. The first valid one wins.
pub fn parse_timestamp(line: &str) -> Option<u32> {
    let b = line.as_bytes();
    let digit = |i: usize| b.get(i).filter(|c| c.is_ascii_digit()).map(|c| (c - b'0') as u32);
    for i in 0..b.len().saturating_sub(7) {
        if b[i + 2] != b':' || b[i + 5] != b':' || (i > 0 && b[i - 1].is_ascii_digit()) {
            continue;
        }
        let in_bracket = line.get(..i).and_then(|pre| pre.rfind('[').map(|o| !pre[o..].contains(']'))).unwrap_or(false);
        if !(i == 0 || in_bracket) {
            continue;
        }
        let (Some(h1), Some(h2), Some(m1), Some(m2), Some(s1), Some(s2)) =
            (digit(i), digit(i + 1), digit(i + 3), digit(i + 4), digit(i + 6), digit(i + 7))
        else {
            continue;
        };
        let (h, m, s) = (h1 * 10 + h2, m1 * 10 + m2, s1 * 10 + s2);
        if h < 24 && m < 60 && s < 60 {
            return Some(h * 3600 + m * 60 + s);
        }
    }
    None
}

/// What one tail read produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Appended {
    /// Complete lines after `from` (a trailing partial line is left for the next read).
    pub lines: Vec<String>,
    /// Offset to resume from.
    pub next_offset: u64,
    /// The file is now shorter than `from`: truncated or replaced; reading restarted at 0.
    pub restarted: bool,
    /// `max_bytes` stopped the read before the end of the file: more is waiting.
    pub capped: bool,
}

impl Appended {
    /// Nothing more to read for now: read to the end, at most a partial line waiting for its
    /// newline. False when the budget cut the read short, so the file is retried next pass.
    pub fn caught_up(&self) -> bool {
        !self.capped
    }
}

/// Reads at most `max_bytes` appended after `from`, then closes the file. Opened read-only with
/// std's default share mode (read|write|delete), so the writer is never blocked.
///
/// Only complete lines are returned. A newline-less run of at least `long_line` bytes (the full
/// per-tick cap) is taken whole as one over-long line; a shorter piece cut off by a small
/// `max_bytes` (leftover budget) stays unread, so a line is never split in two.
pub fn read_appended(path: &Path, from: u64, max_bytes: u64, long_line: u64) -> io::Result<Appended> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let (from, restarted) = if len < from { (0, true) } else { (from, false) };
    let want = (len - from).min(max_bytes);
    let capped = len - from > max_bytes;
    if want == 0 {
        return Ok(Appended { lines: Vec::new(), next_offset: from, restarted, capped });
    }
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity(want as usize);
    f.take(want).read_to_end(&mut buf)?;
    let end = match buf.iter().rposition(|&c| c == b'\n') {
        Some(nl) => nl + 1,
        None if capped && buf.len() as u64 >= long_line.max(1) => buf.len(),
        None => 0,
    };
    Ok(Appended { lines: split_lines(&buf[..end]), next_offset: from + end as u64, restarted, capped })
}

/// The last `max_bytes` of a file as complete lines (a leading partial line dropped), plus the
/// file length: establishes a log's last line without reading its history.
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<(Vec<String>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let from = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity((len - from) as usize);
    f.take(len - from).read_to_end(&mut buf)?;
    let start = if from > 0 { buf.iter().position(|&c| c == b'\n').map_or(buf.len(), |p| p + 1) } else { 0 };
    Ok((split_lines(&buf[start..]), from + buf.len() as u64))
}

/// Complete lines in bytes `[from, to)` (a leading partial line dropped when `from > 0`): the
/// history before a resume offset, read once to rebuild in-memory state after a restart.
pub fn read_range(path: &Path, from: u64, to: u64) -> io::Result<Vec<String>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity(to.saturating_sub(from) as usize);
    f.take(to.saturating_sub(from)).read_to_end(&mut buf)?;
    let start = if from > 0 { buf.iter().position(|&c| c == b'\n').map_or(buf.len(), |p| p + 1) } else { 0 };
    let end = buf.iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1).max(start);
    Ok(split_lines(&buf[start..end]))
}

/// Reads a whole small file (at most `cap` bytes), closing it at once.
pub fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let f = File::open(path)?;
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    Ok(buf)
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The last meaningful line of `lines` (trimmed) and its kind.
pub fn last_meaningful(lines: &[String]) -> Option<(String, LineKind)> {
    lines
        .iter()
        .rev()
        .map(|l| (l, classify_line(l)))
        .find(|(_, k)| k.is_meaningful())
        .map(|(l, k)| (l.trim().to_string(), k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_real_run_lines() {
        let c = classify_line;
        assert_eq!(c("[05:56:27] === stage wnr finished"), LineKind::Completion);
        assert_eq!(c("[05:56:27] wnr marlowe-valiance-22b-wnr6 done (1504s)"), LineKind::Completion);
        assert_eq!(
            c("[05:56:07] wnr marlowe-valiance-22b-wnr6: step 0.25 x Newton(cv): held-out KL 0.16401, top-1 88.49%"),
            LineKind::Progress
        );
        assert_eq!(c("C:\\x\\vstages.py:620: FutureWarning: You are using `torch.load` ... error"), LineKind::Other);
        assert_eq!(c("  W = torch.load(wpath)"), LineKind::Other);
        assert_eq!(c("Traceback (most recent call last):"), LineKind::Traceback);
        assert_eq!(c("torch.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.00 GiB"), LineKind::Oom);
        assert_eq!(c("RuntimeError: shape mismatch"), LineKind::Error);
        assert_eq!(c("[09:15:41] ALERT orchestrator (pid 24752) is dead with the queue unfinished -> relaunching"), LineKind::Error);
        assert_eq!(c("Killed"), LineKind::Killed);
        assert_eq!(c("[05:19:34] step 120 loss nan lr 1e-4"), LineKind::NonFinite);
        assert_eq!(c("{'loss': inf, 'step': 3}"), LineKind::NonFinite);
        assert_eq!(c("held-out error 0.12 at step 40"), LineKind::Progress);
        assert_eq!(c("information about the run"), LineKind::Progress);
        assert_eq!(c(" 45%|████▌     | 45/100 [00:10<00:12, 4.50it/s]"), LineKind::Other);
        assert_eq!(c("   "), LineKind::Other);
        assert_eq!(LineKind::Oom.severity(), Some(Severity::Alarm));
        assert_eq!(LineKind::Completion.severity(), Some(Severity::Good));
    }

    #[test]
    fn endings_from_last_lines() {
        let e = ending;
        assert_eq!(e("ship: marlowe-valiance-22b-wnr-final-truewidth-q4_k_s.gguf written, check OK 06:54"), Ending::Success);
        assert_eq!(e("llama_model_quantize_impl: quant size  = 14853.78 MiB (4.63 BPW)"), Ending::Success);
        assert_eq!(e("[05:56:27] === stage wnr finished"), Ending::Success);
        assert_eq!(e("saved model to out/final"), Ending::Success);
        assert_eq!(e("checkpoint could not be written"), Ending::Unknown);
        assert_eq!(e("126.47.966.376 I slot print_timing: id  0 | task 62617 | n_gen =   2686, tg =   8.93 t/s"), Ending::Unknown);
        assert_eq!(e("torch.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.00 GiB"), Ending::Failure("out of memory".into()));
        assert_eq!(e("RuntimeError: shape mismatch"), Ending::Failure("RuntimeError".into()));
        assert_eq!(e("[03:10:00] step 120 loss nan lr 1e-4"), Ending::Failure("NaN/inf".into()));
        assert_eq!(e("worker exited with code 137"), Ending::Failure("exit code 137".into()));
        assert_eq!(e("process finished with exit code 0"), Ending::Success);
        assert_eq!(e("Killed"), Ending::Failure("killed".into()));
        // Never success when the line says otherwise.
        assert_eq!(e("ship: x-q4_k_s.gguf written, check FAILED"), Ending::Failure("its check failed".into()));
        assert_eq!(e("ship: x-q4_k_s.gguf written, check failed"), Ending::Failure("its check failed".into()));
        assert_eq!(e("Not written: x.gguf"), Ending::Unknown);
        assert_eq!(e("x.gguf wasn't saved"), Ending::Unknown);
        assert_eq!(e("Error writing x.gguf"), Ending::Failure("an error".into()));
        assert_eq!(e("done, 3 tests failed"), Ending::Failure("its check failed".into()));
        assert_eq!(e("completed with errors"), Ending::Failure("errors".into()));
        assert_eq!(e("training not finished"), Ending::Unknown);
        assert_eq!(e("aborted after step 120"), Ending::Failure("aborted".into()));
        assert_eq!(e("done: 12 passed, 0 failed"), Ending::Success);
        assert_eq!(e("saved x.pt, no errors"), Ending::Success);
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("[05:19:34] wnr step"), Some(5 * 3600 + 19 * 60 + 34));
        assert_eq!(parse_timestamp("[2026-10-09 23:01:02,123] INFO"), Some(23 * 3600 + 62));
        assert_eq!(parse_timestamp("07:00:00 start"), Some(7 * 3600));
        assert_eq!(parse_timestamp("ratio 12:34:56 mid-line"), None);
        assert_eq!(parse_timestamp("[25:00:00] bad"), None);
        assert_eq!(parse_timestamp("no time"), None);
    }

    #[test]
    fn tails_only_appended_complete_lines() {
        let dir = std::env::temp_dir().join(format!("sentinel-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.log");
        std::fs::write(&p, "one\ntwo\npart").unwrap();
        const CAP: u64 = 1 << 20;
        let a = read_appended(&p, 0, CAP, CAP).unwrap();
        assert_eq!(a.lines, vec!["one", "two"]);
        assert_eq!(a.next_offset, 8);
        assert!(a.caught_up(), "a trailing partial line waits for its newline");
        std::fs::write(&p, "one\ntwo\npartial\r\nthree\n").unwrap();
        let b = read_appended(&p, a.next_offset, CAP, CAP).unwrap();
        assert_eq!(b.lines, vec!["partial", "three"]);
        // A capped read leaves the rest for later.
        let c = read_appended(&p, 0, 5, CAP).unwrap();
        assert_eq!((c.lines.clone(), c.next_offset), (vec!["one".to_string()], 4));
        // Leftover budget shorter than the next line: nothing taken, and the file is not done.
        let e = read_appended(&p, 8, 4, CAP).unwrap();
        assert_eq!((e.lines.len(), e.next_offset), (0, 8));
        assert!(!e.caught_up());
        // Only a newline-less run as long as the full cap is taken whole.
        let f = read_appended(&p, 8, 4, 4).unwrap();
        assert_eq!((f.lines.clone(), f.next_offset), (vec!["part".to_string()], 12));
        // Truncation restarts at 0.
        std::fs::write(&p, "new\n").unwrap();
        let d = read_appended(&p, b.next_offset, CAP, CAP).unwrap();
        assert!(d.restarted);
        assert_eq!(d.lines, vec!["new"]);
        let (tail, end) = read_tail(&p, 2).unwrap();
        assert!(tail.is_empty());
        assert_eq!(end, 4);
        assert_eq!(read_tail(&p, 100).unwrap().0, vec!["new"]);
        assert_eq!(last_meaningful(&["done".into(), "  x".into()]), Some(("done".into(), LineKind::Completion)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
