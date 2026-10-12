//! `grep`: ripgrep's engine behind Claude Code's parameters (design 2.1 T11).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch};
use marlowe_agent::ToolOutcome;
use serde_json::Value;

use super::glob::unknown_param;
use super::walk::{collect, skipped_notice, Filters};
use super::{join_within, lenient_bool, lenient_int, Page, SearchEnv, RESULT_BYTES};

/// The parameters as the model should send them; the bare spellings are accepted too.
const PARAMS: [&str; 19] = [
    "pattern",
    "path",
    "glob",
    "type",
    "output_mode",
    "-A",
    "-B",
    "-C",
    "-i",
    "-n",
    "multiline",
    "head_limit",
    "offset",
    "A",
    "B",
    "C",
    "i",
    "n",
    "context",
];

/// Largest context a call may ask for.
const MAX_CONTEXT: i64 = 200;

/// Characters of one result line kept.
const MAX_LINE_CHARS: usize = 500;

/// The compiled-regex size ceiling (NFA and lazy DFA).
const REGEX_LIMIT: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Files,
    Content,
    Count,
}

#[derive(Clone, Copy)]
struct Opts {
    mode: Mode,
    before: usize,
    after: usize,
    numbers: bool,
    multiline: bool,
}

/// One row of a file's content output.
#[derive(Clone)]
enum Row {
    Line { n: u64, matched: bool, text: String },
    Break,
}

#[derive(Default)]
struct FileHits {
    rows: Vec<Row>,
    /// Lines that matched (all of them in `Count` mode, up to the cap otherwise).
    lines: usize,
    binary: bool,
    unreadable: bool,
    /// The file was left unread past the row cap: it may hold more matches than `rows` has.
    capped: bool,
}

struct Collector {
    opts: Opts,
    /// Content rows kept per file before it stops being read.
    cap: usize,
    hits: FileHits,
}

fn line_text(bytes: &[u8]) -> String {
    let mut b = bytes;
    while let Some((&last, rest)) = b.split_last() {
        if last == b'\n' || last == b'\r' {
            b = rest;
        } else {
            break;
        }
    }
    let s = String::from_utf8_lossy(b);
    if s.chars().count() <= MAX_LINE_CHARS {
        return s.into_owned();
    }
    let kept: String = s.chars().take(MAX_LINE_CHARS).collect();
    format!("{kept}… [line cut, {} more characters]", s.chars().count() - MAX_LINE_CHARS)
}

impl Sink for Collector {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        match self.opts.mode {
            Mode::Files => {
                self.hits.lines += 1;
                Ok(false)
            }
            Mode::Count => {
                self.hits.lines += mat.lines().count().max(1);
                Ok(true)
            }
            Mode::Content => {
                let first = mat.line_number().unwrap_or(0);
                for (i, line) in mat.lines().enumerate() {
                    self.hits.lines += 1;
                    self.hits.rows.push(Row::Line { n: first + i as u64, matched: true, text: line_text(line) });
                }
                self.hits.capped = self.hits.rows.len() >= self.cap;
                Ok(!self.hits.capped)
            }
        }
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let first = ctx.line_number().unwrap_or(0);
        let bytes = ctx.bytes();
        // A context block is one line in line-by-line mode; split anyway, as rg prints per line.
        for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            self.hits.rows.push(Row::Line { n: first + i as u64, matched: false, text: line_text(line) });
        }
        let room = self.hits.rows.len() < self.cap.saturating_add(self.opts.after);
        self.hits.capped |= !room;
        Ok(room)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.hits.rows.push(Row::Break);
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, Self::Error> {
        self.hits.binary = true;
        Ok(true)
    }

    fn finish(&mut self, _: &Searcher, finish: &SinkFinish) -> Result<(), Self::Error> {
        if finish.binary_byte_offset().is_some() {
            self.hits.binary = true;
        }
        Ok(())
    }
}

fn build_searcher(o: &Opts) -> Searcher {
    let mut b = SearcherBuilder::new();
    b.line_number(true)
        .multi_line(o.multiline)
        .before_context(o.before)
        .after_context(o.after)
        .binary_detection(BinaryDetection::quit(0))
        .line_terminator(LineTerminator::crlf());
    b.build()
}

fn search_one(env: &SearchEnv, searcher: &mut Searcher, matcher: &RegexMatcher, path: &Path, o: Opts, cap: usize) -> FileHits {
    let mut sink = Collector { opts: o, cap, hits: FileHits::default() };
    let res = if env.is_local() {
        searcher.search_path(matcher, path, &mut sink)
    } else {
        match env.port.read(path) {
            Ok(bytes) => searcher.search_slice(matcher, &bytes, &mut sink),
            Err(e) => {
                sink.hits.unreadable = true;
                let _ = e;
                Ok(())
            }
        }
    };
    if res.is_err() {
        sink.hits.unreadable = true;
    }
    sink.hits
}

/// Files per parallel batch when a call can stop early: batches are searched in order, so what a
/// call returns (and whether it says "at least") depends only on the files, never on timing.
const BATCH: usize = 64;

/// Searches `files` in parallel, a batch at a time in path order. With `stop_after` rows, it
/// stops after the batch that reaches that many; the flag says later files were not searched.
fn run(
    env: &SearchEnv,
    files: &[PathBuf],
    matcher: &RegexMatcher,
    o: Opts,
    stop_after: Option<usize>,
) -> (Vec<Option<FileHits>>, bool) {
    let cap = stop_after.unwrap_or(usize::MAX);
    let mut out: Vec<Option<FileHits>> = Vec::with_capacity(files.len());
    let mut rows = 0usize;
    let batch = if stop_after.is_some() { BATCH } else { files.len().max(1) };
    for chunk in files.chunks(batch) {
        let results: Vec<Mutex<Option<FileHits>>> = chunk.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8)).min(chunk.len());
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    let mut searcher = build_searcher(&o);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = chunk.get(i) else { break };
                        let hits = search_one(env, &mut searcher, matcher, path, o, cap);
                        *results[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(hits);
                    }
                });
            }
        });
        for m in results {
            let hits = m.into_inner().unwrap_or_else(|p| p.into_inner());
            rows += hits.as_ref().map_or(0, |h| h.rows.len());
            out.push(hits);
        }
        if rows >= cap {
            break;
        }
    }
    let early = out.len() < files.len();
    (out, early)
}

fn parse_context(args: &Value, keys: &[&str]) -> Result<Option<usize>, ToolOutcome> {
    for k in keys {
        if let Some(v) = args.get(*k).filter(|v| !v.is_null()) {
            return match lenient_int(v) {
                Some(n) if (0..=MAX_CONTEXT).contains(&n) => Ok(Some(n as usize)),
                Some(n) => Err(ToolOutcome::error(format!(
                    "`{k}` is {n}, and it must be a whole number from 0 to {MAX_CONTEXT}. It was NOT reduced to fit."
                ))),
                None => Err(ToolOutcome::error(format!("`{k}` must be a whole number from 0 to {MAX_CONTEXT}."))),
            };
        }
    }
    Ok(None)
}

fn flag(args: &Value, keys: &[&str]) -> Result<Option<bool>, ToolOutcome> {
    for k in keys {
        if let Some(v) = args.get(*k).filter(|v| !v.is_null()) {
            return lenient_bool(v)
                .map(Some)
                .ok_or_else(|| ToolOutcome::error(format!("`{k}` must be true or false")));
        }
    }
    Ok(None)
}

pub fn grep(env: &SearchEnv, args: &Value) -> ToolOutcome {
    // The bare spellings (`A`, `context`) are accepted; the refusal lists the canonical names.
    if let Some(bad) = unknown_param(args, &PARAMS, &PARAMS[..13], "grep") {
        return bad;
    }
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return ToolOutcome::error("`pattern` is required");
    };
    if pattern.is_empty() {
        return ToolOutcome::error(
            "`pattern` is empty, which matches every line of every file. Give the regular expression to search \
             for, or use `glob` to list files.",
        );
    }
    let mode = match args.get("output_mode") {
        None | Some(Value::Null) => Mode::Files,
        Some(Value::String(s)) => match s.as_str() {
            "files_with_matches" | "files" => Mode::Files,
            "content" => Mode::Content,
            "count" => Mode::Count,
            other => {
                return ToolOutcome::error(format!(
                    "`output_mode` \"{other}\" is not one of files_with_matches (default: just the paths), content \
                     (the matching lines), count (matching lines per file)."
                ))
            }
        },
        Some(_) => return ToolOutcome::error("`output_mode` must be a string"),
    };
    let (ci, numbers, multiline) = match (flag(args, &["-i", "i"]), flag(args, &["-n", "n"]), flag(args, &["multiline"])) {
        (Ok(i), Ok(n), Ok(m)) => (i.unwrap_or(false), n.unwrap_or(true), m.unwrap_or(false)),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
    };
    let (a, b, c) = match (
        parse_context(args, &["-A", "A"]),
        parse_context(args, &["-B", "B"]),
        parse_context(args, &["-C", "C", "context"]),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
    };
    let opts = Opts {
        mode,
        before: b.or(c).unwrap_or(0),
        after: a.or(c).unwrap_or(0),
        numbers,
        multiline,
    };
    let page = match Page::parse(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let base = match env.base(args) {
        Ok(b) => b,
        Err(e) => return e,
    };
    if env.port.metadata(&base).is_err() {
        return ToolOutcome::error(format!("`path` {} does not exist", env.shown(&base)));
    }

    let mut mb = RegexMatcherBuilder::new();
    mb.case_insensitive(ci)
        .multi_line(true)
        .dot_matches_new_line(multiline)
        .crlf(true)
        .size_limit(REGEX_LIMIT)
        .dfa_size_limit(REGEX_LIMIT);
    if multiline {
        // `crlf` made CRLF the matcher's line terminator, which forbids matching across lines.
        mb.line_terminator(None);
    }
    let matcher = match mb.build(pattern) {
        Ok(m) => m,
        Err(e) => {
            let mut why = e.to_string().replace("the --multiline flag (or -U for short)", "the `multiline` parameter");
            if why.contains("is not allowed in a regex") {
                why.push_str(" (set `multiline` to true to match across lines)");
            }
            return ToolOutcome::error(format!(
                "`pattern` is not a valid regular expression, and it was NOT searched for as plain text instead: \
                 {why}\nTo search for text that CONTAINS one of `. * + ? ( ) [ ] {{ }} | ^ $ \\`, put a backslash \
                 before each one."
            ));
        }
    };

    let mut filters = Filters::default();
    match args.get("glob") {
        None | Some(Value::Null) => {}
        Some(Value::String(g)) if !g.trim().is_empty() => match filters.with_globs(&base, env.root, g) {
            Ok(f) => filters = f,
            Err(e) => return ToolOutcome::error(e),
        },
        _ => {
            return ToolOutcome::error(
                "`glob` is empty, which matches no file at all. Give a pattern like `*.rs`, or omit `glob` to search \
                 every file.",
            )
        }
    }
    match args.get("type") {
        None | Some(Value::Null) => {}
        Some(Value::String(t)) if !t.trim().is_empty() => match filters.with_type(t.trim()) {
            Ok(f) => filters = f,
            Err(e) => return ToolOutcome::error(e),
        },
        _ => return ToolOutcome::error("`type` must be a file type name such as `rust` or `py`"),
    }

    let walked = collect(env, &base, &filters);
    let searched = walked.files.len();
    let stop_after = (mode == Mode::Content).then(|| page.offset.saturating_add(page.limit).saturating_add(1));
    let (results, early) = run(env, &walked.files, &matcher, opts, stop_after);
    // Later files were not searched, or a file was not read to its end: the totals are lower bounds.
    let early = early || results.iter().flatten().any(|h| h.capped);

    let mut binary = 0usize;
    let mut unreadable = 0usize;
    let mut with_hits: Vec<(&PathBuf, &FileHits)> = Vec::new();
    for (path, hits) in walked.files.iter().zip(&results) {
        let Some(h) = hits else { continue };
        binary += usize::from(h.binary);
        unreadable += usize::from(h.unreadable);
        if h.lines > 0 {
            with_hits.push((path, h));
        }
    }

    let (mut body, summary) = match mode {
        Mode::Files => {
            let mut v: Vec<(Option<SystemTime>, String)> = with_hits
                .iter()
                .map(|(p, _)| (env.port.metadata(p).ok().and_then(|m| m.modified), env.shown(p)))
                .collect();
            v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let total = v.len();
            let shown: Vec<String> = page.slice(&v).iter().map(|(_, p)| p.clone()).collect();
            let (mut text, fit) = join_within(&shown, RESULT_BYTES);
            if fit < shown.len() {
                text.push_str(&format!("\n[output cut at {RESULT_BYTES} bytes after {fit} paths]"));
            } else if let Some(more) = page.more(shown.len(), total, false) {
                text.push_str(&format!("\n{more}"));
            }
            (text, format!("{total} files"))
        }
        Mode::Count => {
            let v: Vec<String> = with_hits.iter().map(|(p, h)| format!("{}:{}", env.shown(p), h.lines)).collect();
            let total = v.len();
            let shown = page.slice(&v);
            let (mut text, fit) = join_within(shown, RESULT_BYTES);
            if fit < shown.len() {
                text.push_str(&format!("\n[output cut at {RESULT_BYTES} bytes after {fit} files]"));
            } else if let Some(more) = page.more(shown.len(), total, false) {
                text.push_str(&format!("\n{more}"));
            }
            let lines: usize = with_hits.iter().map(|(_, h)| h.lines).sum();
            (text, format!("{lines} matches · {total} files"))
        }
        Mode::Content => {
            let mut rows: Vec<String> = Vec::new();
            let mut matches = 0usize;
            let mut files_hit = 0usize;
            for (path, h) in &with_hits {
                if (opts.before > 0 || opts.after > 0) && !rows.is_empty() {
                    rows.push("--".into());
                }
                files_hit += 1;
                matches += h.lines;
                let shown = env.shown(path);
                for r in &h.rows {
                    rows.push(match r {
                        Row::Break => "--".to_string(),
                        Row::Line { n, matched, text } => {
                            let sep = if *matched { ':' } else { '-' };
                            if opts.numbers {
                                format!("{shown}{sep}{n}{sep}{text}")
                            } else {
                                format!("{shown}{sep}{text}")
                            }
                        }
                    });
                }
            }
            let total = rows.len();
            let shown = page.slice(&rows);
            let (mut text, fit) = join_within(shown, RESULT_BYTES);
            if fit < shown.len() {
                text.push_str(&format!(
                    "\n[output cut at {RESULT_BYTES} bytes after {fit} lines; tighten `pattern`, set `glob` or `type`, \
                     or use `offset`]"
                ));
            } else if let Some(more) = page.more(shown.len(), total, early) {
                text.push_str(&format!("\n{more}"));
            }
            let at_least = if early { "at least " } else { "" };
            (text, format!("{at_least}{matches} matches · {files_hit} files"))
        }
    };

    if with_hits.is_empty() {
        body = if searched == 0 {
            format!(
                "no files to search under {} (files ignored by .gitignore are skipped, and so are those `glob`/`type` \
                 exclude; name an ignored directory as `path` to search it). A `glob` with `/` is relative to `path`.",
                env.shown_dir(&base)
            )
        } else {
            let noun = if searched == 1 { "file" } else { "files" };
            format!("no line matched `{pattern}` in the {searched} {noun} searched")
        };
    }
    if binary > 0 {
        body.push_str(&format!(
            "\n[{binary} file(s) contain a NUL byte: they were searched only up to it, as ripgrep does.]"
        ));
    }
    if unreadable > 0 {
        body.push_str(&format!("\n[{unreadable} file(s) could not be read and were not searched.]"));
    }
    if walked.truncated {
        body.push_str(&format!(
            "\n[the search stopped after {} files and there is more under this path. Narrow it with `glob` or `type`, \
             or search a subdirectory.]",
            super::walk::FILE_CAP
        ));
    }
    body.push_str(&skipped_notice(&walked.skipped));
    ToolOutcome::ok(body, summary)
}
