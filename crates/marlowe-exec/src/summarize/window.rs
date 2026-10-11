//! Summarising a log that is too big to hold: a bounded pass over the file.
//!
//! [`condense`] streams the log once and keeps the head, the tail, and (between them) the lines
//! [`super::summarize`] would choose to keep from a failure or a warning, each with its
//! neighbours. What it returns is a few MiB at most however large the log is, and goes through
//! [`super::summarize`] like any other text. Gaps are marked with the lines they hide.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// A log up to this size is read whole.
pub const WHOLE: u64 = 2 << 20;

const HEAD: usize = 128 << 10;
const TAIL: usize = 1 << 20;
const MID: usize = 512 << 10;
/// A single line is cut here (the summary clips lines at a few hundred bytes anyway).
const LINE: usize = 16 << 10;
/// Lines after a matching one that go with it (as in [`super::summarize`]).
const AFTER: usize = 8;

/// A condensed log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condensed {
    /// Cleaned lines: the head, kept blocks from the middle, the tail, gaps marked.
    pub text: String,
    /// Lines in the whole log.
    pub total_lines: usize,
}

struct Mid {
    re: &'static regex::Regex,
    kept: Vec<(usize, String)>,
    bytes: usize,
    prev: Option<(usize, String)>,
    after: usize,
}

impl Mid {
    fn offer(&mut self, at: usize, line: String) {
        if self.after > 0 && !line.trim().is_empty() {
            self.after -= 1;
            self.keep(at, line.clone());
        } else if self.re.is_match(&line) {
            self.after = AFTER;
            if let Some((p, l)) = self.prev.take() {
                if self.kept.last().is_none_or(|(k, _)| *k != p) {
                    self.keep(p, l);
                }
            }
            self.keep(at, line.clone());
        } else {
            self.after = 0;
        }
        self.prev = Some((at, line));
    }

    fn keep(&mut self, at: usize, line: String) {
        if self.bytes + line.len() + 1 <= MID {
            self.bytes += line.len() + 1;
            self.kept.push((at, line));
        }
    }
}

/// The condensed log at `path`, `failed` choosing failure blocks over warnings for the middle.
/// `None` when the file cannot be read.
pub fn condense(path: &Path, failed: bool) -> Option<Condensed> {
    let mut f = File::open(path).ok()?;
    let mut head: Vec<String> = Vec::new();
    let mut head_bytes = 0usize;
    let mut head_open = true;
    let mut tail: VecDeque<(usize, String)> = VecDeque::new();
    let mut tail_bytes = 0usize;
    let mut mid = Mid {
        re: if failed { super::salient() } else { super::warning() },
        kept: Vec::new(),
        bytes: 0,
        prev: None,
        after: 0,
    };
    let mut n = 0usize;
    let mut cur: Vec<u8> = Vec::new();
    let mut over = 0usize;
    let mut buf = vec![0u8; 256 << 10];

    let mut line_done = |cur: &mut Vec<u8>, over: &mut usize| {
        let mut text = super::clean(&String::from_utf8_lossy(cur));
        if *over > 0 {
            text.push_str(&format!("… [{} more bytes]", *over));
        }
        cur.clear();
        *over = 0;
        let at = n;
        n += 1;
        if head_open && head_bytes + text.len() < HEAD {
            head_bytes += text.len() + 1;
            head.push(text);
            return;
        }
        head_open = false;
        tail_bytes += text.len() + 1;
        tail.push_back((at, text));
        while tail_bytes > TAIL {
            let Some((i, l)) = tail.pop_front() else { break };
            tail_bytes -= l.len() + 1;
            mid.offer(i, l);
        }
    };

    loop {
        let got = f.read(&mut buf).ok()?;
        if got == 0 {
            break;
        }
        let mut rest = &buf[..got];
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            push_capped(&mut cur, &mut over, &rest[..nl]);
            line_done(&mut cur, &mut over);
            rest = &rest[nl + 1..];
        }
        push_capped(&mut cur, &mut over, rest);
    }
    if !cur.is_empty() || over > 0 {
        line_done(&mut cur, &mut over);
    }

    let mut out = String::new();
    let mut next = 0usize;
    let emit = |at: usize, line: &str, out: &mut String, next: &mut usize| {
        if at > *next {
            out.push_str(&format!("[... {} lines omitted ...]\n", at - *next));
        }
        out.push_str(line);
        out.push('\n');
        *next = at + 1;
    };
    for (i, l) in head.iter().enumerate() {
        emit(i, l, &mut out, &mut next);
    }
    for (i, l) in &mid.kept {
        emit(*i, l, &mut out, &mut next);
    }
    for (i, l) in &tail {
        emit(*i, l, &mut out, &mut next);
    }
    out.truncate(out.trim_end().len());
    Some(Condensed { text: out, total_lines: n })
}

fn push_capped(cur: &mut Vec<u8>, over: &mut usize, bytes: &[u8]) {
    let room = LINE.saturating_sub(cur.len());
    let take = bytes.len().min(room);
    cur.extend_from_slice(&bytes[..take]);
    *over += bytes.len() - take;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(text: &str) -> tempfile_path::Tmp {
        tempfile_path::Tmp::new(text)
    }

    mod tempfile_path {
        pub struct Tmp(pub std::path::PathBuf);
        impl Tmp {
            pub fn new(text: &str) -> Self {
                let p = std::env::temp_dir().join(format!("marlowe-condense-{}-{:?}.log", std::process::id(), std::thread::current().id()));
                std::fs::write(&p, text).unwrap();
                Tmp(p)
            }
        }
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    #[test]
    fn a_big_log_keeps_its_head_tail_and_the_failure_in_the_middle() {
        let mut text = String::from("starting the build\n");
        for i in 0..400_000 {
            text.push_str(&format!("compiling unit {i}\n"));
            if i == 123_456 {
                text.push_str("error[E0432]: unresolved import `foo`\n  --> src/lib.rs:3:5\n\n");
            }
        }
        text.push_str("error: could not compile `app`\n");
        let t = log(&text);
        assert!(text.len() as u64 > WHOLE * 3);
        let c = condense(&t.0, true).unwrap();
        assert!(c.text.len() < 2 << 20, "{}", c.text.len());
        assert_eq!(c.total_lines, 400_000 + 1 + 3 + 1);
        assert!(c.text.starts_with("starting the build"));
        assert!(c.text.contains("error[E0432]: unresolved import `foo`") && c.text.contains("src/lib.rs:3:5"));
        assert!(c.text.ends_with("error: could not compile `app`"));
        assert!(c.text.contains("lines omitted"));
    }

    #[test]
    fn an_enormous_line_and_a_missing_final_newline_are_bounded() {
        let t = log(&format!("{}\nlast line without newline", "x".repeat(5 << 20)));
        let c = condense(&t.0, false).unwrap();
        assert!(c.text.len() < 40 << 10, "{}", c.text.len());
        assert!(c.text.contains("more bytes]") && c.text.ends_with("last line without newline"));
        assert_eq!(c.total_lines, 2);
    }
}
