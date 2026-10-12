//! The edit-reliability suite (gate A5): a live model makes small, fully specified changes to
//! small files through one edit format at a time, and `marlowe_code::edit::bench` scores it
//! (first attempt applies; the intended text reached within two retries) with the same engines
//! the tools use. The file is shown the way `read` shows it for that format, a failed call's
//! error goes back as the tool result, and the best formats can be recorded for the model
//! (`--apply`, `Library::set_edit_formats`).

use std::collections::BTreeMap;
use std::time::Instant;

use marlowe_code::edit::bench::{self, BenchCase, CaseResult, Format, Gate, GATE_FRONTIER, GATE_LOCAL, MAX_RETRIES};
use marlowe_code::editlines::{anchored, line_hash};
use marlowe_model::{CancelToken, ChatRequest, Message, Part, Provider, Role, SystemBlock, ToolResult, ToolSpec};
use serde::Serialize;
use serde_json::{json, Value};

const SYSTEM: &str = "You edit files with the single tool you are given. Make exactly the change asked, \
nothing else. One tool call per attempt, no commentary. Paths are relative to the repository root.";

const MAX_CONSECUTIVE_ERRORS: usize = 3;

const RUST_SRC: &str = "use std::collections::HashMap;

pub struct Cache {
    entries: HashMap<String, u64>,
    capacity: usize,
}

impl Cache {
    pub fn new(capacity: usize) -> Self {
        Cache { entries: HashMap::new(), capacity }
    }

    pub fn put(&mut self, key: &str, value: u64) {
        if self.entries.len() >= self.capacity {
            self.entries.clear();
        }
        self.entries.insert(key.to_string(), value);
    }

    pub fn lookup(&self, key: &str) -> Option<u64> {
        self.entries.get(key).copied()
    }

    pub fn default_capacity() -> usize {
        128
    }
}
";

const PY_SRC: &str = "import math

RETRIES = 3
BACKOFF_SECONDS = 20


def mean(values):
    if not values:
        return 0.0
    return sum(values) / len(values)


def clamp(x, low, high):
    return max(low, min(high, x))


def variance(values):
    m = mean(values)
    return sum((v - m) ** 2 for v in values) / len(values)


def norm(vec):
    return math.sqrt(sum(v * v for v in vec))
";

const TS_SRC: &str = "export interface Options {
  retries: number;
  verbose: boolean;
}

const DEFAULT_TIMEOUT_MS = 4500;

export function parseOptions(raw: string): Options {
  const data = JSON.parse(raw);
  return { retries: data.retries ?? 2, verbose: !!data.verbose };
}

export function describe(opts: Options): string {
  return `retries=${opts.retries} verbose=${opts.verbose}`;
}

export function shouldRetry(opts: Options, attempt: number): boolean {
  return attempt < opts.retries;
}
";

const GO_SRC: &str = "package queue

import \"sync\"

type Queue struct {
	mu    sync.Mutex
	items []int
}

func New() *Queue {
	return &Queue{}
}

func (q *Queue) Push(v int) {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.items = append(q.items, v)
}

func (q *Queue) Pop() (int, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()
	if len(q.items) == 0 {
		return 0, false
	}
	v := q.items[0]
	q.items = q.items[1:]
	return v, true
}

const MaxDepth = 64
";

const SOURCES: [(&str, &str, &str); 4] = [
    ("src/cache.rs", RUST_SRC, "//"),
    ("lib/stats.py", PY_SRC, "#"),
    ("src/options.ts", TS_SRC, "//"),
    ("queue/queue.go", GO_SRC, "//"),
];

/// A change to make, with the right call for each format (the oracle the tests check and the
/// generator's own proof that `intended` is reachable).
#[derive(Debug, Clone)]
pub struct EditTask {
    pub id: String,
    pub path: &'static str,
    pub original: String,
    pub intended: String,
    pub instruction: String,
    /// The right call for each format: the generator's proof that `intended` is reachable (the
    /// tests apply it) and what a test double answers with.
    #[allow(dead_code)]
    pub oracle: BTreeMap<Format, Value>,
}

fn tool_name(f: Format) -> &'static str {
    match f {
        Format::Edit => "edit",
        Format::EditLines => "edit_lines",
        Format::Write => "write",
    }
}

fn format_name(f: Format) -> &'static str {
    tool_name(f)
}

/// Splitmix64: a seeded, portable shuffle key.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Lines (without terminator) that occur exactly once in the file, non-blank.
fn unique_lines(lines: &[&str]) -> Vec<usize> {
    (0..lines.len()).filter(|&i| !lines[i].trim().is_empty() && lines.iter().filter(|l| l.trim() == lines[i].trim()).count() == 1).collect()
}

struct Draft {
    kind: &'static str,
    path: &'static str,
    original: String,
    intended: String,
    instruction: String,
    /// (edit args, edit_lines args)
    edit: Value,
    lines: Value,
}

fn join(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// Every task the sources allow, in a fixed order.
fn drafts() -> Vec<Draft> {
    let mut out = Vec::new();
    for (path, src, comment) in SOURCES {
        let lines: Vec<&str> = src.lines().collect();
        let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let unique = unique_lines(&lines);
        let anchor = |i: usize| format!("{}:{}", i + 1, line_hash(lines[i]));
        for &i in &unique {
            let line = lines[i];
            let t = line.trim();
            // 1. A number on a line.
            if !t.starts_with(comment) {
                let b = line.as_bytes();
                let digits = (0..b.len()).find(|&k| b[k].is_ascii_digit() && (k == 0 || !(b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_')));
                if let Some(k) = digits {
                    let end = (k..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
                    let old = &line[k..end];
                    if end == b.len() || !(b[end].is_ascii_alphabetic() || b[end] == b'_' || b[end] == b'.') {
                        let new = (old.parse::<u64>().unwrap_or(1) + 7).to_string();
                        let new_line = format!("{}{}{}", &line[..k], new, &line[end..]);
                        let mut next = owned.clone();
                        next[i] = new_line.clone();
                        out.push(Draft {
                            kind: "number",
                            path,
                            original: src.to_string(),
                            intended: join(&next),
                            instruction: format!("On the line `{t}`, change the number {old} to {new}."),
                            edit: json!({"path": path, "old_string": line, "new_string": new_line}),
                            lines: json!({"path": path, "anchor": anchor(i), "op": "replace", "text": new_line}),
                        });
                    }
                }
            }
            // 2. A function's name.
            let bare = t.trim_start_matches("pub ").trim_start_matches("export ");
            let name = ["fn ", "def ", "func ", "function "].iter().find_map(|kw| {
                let name: String = bare.strip_prefix(kw)?.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                (!name.is_empty()).then_some(name)
            });
            if let Some(name) = name {
                let new = format!("{name}_v2");
                let new_line = line.replacen(&name, &new, 1);
                let mut next = owned.clone();
                next[i] = new_line.clone();
                out.push(Draft {
                    kind: "rename",
                    path,
                    original: src.to_string(),
                    intended: join(&next),
                    instruction: format!("Rename the function `{name}` to `{new}` on its definition line `{t}` only. Leave every call site alone."),
                    edit: json!({"path": path, "old_string": line, "new_string": new_line}),
                    lines: json!({"path": path, "anchor": anchor(i), "op": "replace", "text": new_line}),
                });
            }
            // 3. A line inserted after this one.
            if i + 1 < lines.len() {
                let text = format!("{comment} reviewed");
                let new_line = format!("{}{}", indent_of(line), text);
                let mut next = owned.clone();
                next.insert(i + 1, new_line.clone());
                out.push(Draft {
                    kind: "insert",
                    path,
                    original: src.to_string(),
                    intended: join(&next),
                    instruction: format!("Directly after the line `{t}` insert one new line, `{text}`, indented exactly like that line."),
                    edit: json!({"path": path, "old_string": line, "new_string": format!("{line}\n{new_line}")}),
                    lines: json!({"path": path, "anchor": anchor(i), "op": "insert_after", "text": new_line}),
                });
            }
            // 4. A line deleted.
            if i > 0 && i + 1 < lines.len() && !t.starts_with(comment) && !matches!(t, "}" | "{" | ")" | "]") {
                let mut next = owned.clone();
                next.remove(i);
                out.push(Draft {
                    kind: "delete",
                    path,
                    original: src.to_string(),
                    intended: join(&next),
                    instruction: format!("Delete the line `{t}`."),
                    edit: json!({"path": path, "old_string": format!("{line}\n"), "new_string": ""}),
                    lines: json!({"path": path, "anchor": anchor(i), "op": "delete"}),
                });
            }
            // 5. Two consecutive lines become one.
            if i + 1 < lines.len() && unique.contains(&(i + 1)) && !t.starts_with(comment) {
                let next_line = lines[i + 1];
                let text = format!("{comment} merged");
                let new_line = format!("{}{}", indent_of(line), text);
                let mut next = owned.clone();
                next.splice(i..=i + 1, [new_line.clone()]);
                out.push(Draft {
                    kind: "merge",
                    path,
                    original: src.to_string(),
                    intended: join(&next),
                    instruction: format!(
                        "Replace the two consecutive lines `{t}` and `{}` with the single line `{text}`, indented like the first.",
                        next_line.trim()
                    ),
                    edit: json!({"path": path, "old_string": format!("{line}\n{next_line}"), "new_string": new_line}),
                    lines: json!({"path": path, "anchor": anchor(i), "end": anchor(i + 1), "op": "replace", "text": new_line}),
                });
            }
        }
    }
    out
}

/// `n` tasks, the same for the same `seed`, spread over the sources and kinds.
pub fn tasks(n: usize, seed: u64) -> Vec<EditTask> {
    let mut all = drafts();
    // Order by a seeded key, then take round-robin by kind so every kind is covered.
    let mut keyed: Vec<(u64, usize)> = (0..all.len()).map(|i| (mix(seed ^ (i as u64 + 1).wrapping_mul(0x1000_0001)), i)).collect();
    keyed.sort();
    let mut by_kind: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (_, i) in keyed {
        by_kind.entry(all[i].kind).or_default().push(i);
    }
    let mut picked: Vec<usize> = Vec::new();
    'rounds: for round in 0.. {
        let mut any = false;
        for list in by_kind.values() {
            if let Some(&i) = list.get(round) {
                any = true;
                picked.push(i);
                if picked.len() >= n {
                    break 'rounds;
                }
            }
        }
        if !any {
            break;
        }
    }
    picked
        .into_iter()
        .enumerate()
        .map(|(k, i)| {
            let d = std::mem::replace(&mut all[i], Draft { kind: "", path: "", original: String::new(), intended: String::new(), instruction: String::new(), edit: Value::Null, lines: Value::Null });
            let oracle = BTreeMap::from([
                (Format::Edit, d.edit),
                (Format::EditLines, d.lines),
                (Format::Write, json!({"path": d.path, "content": d.intended})),
            ]);
            EditTask { id: format!("{k:03}-{}", d.kind), path: d.path, original: d.original, intended: d.intended, instruction: d.instruction, oracle }
        })
        .collect()
}

/// The prompt a model sees for `task` in `format`.
pub fn prompt(task: &EditTask, format: Format) -> String {
    let view = match format {
        Format::EditLines => anchored(&task.original, 1),
        _ => task.original.lines().enumerate().map(|(i, l)| format!("{:>6}\t{l}\n", i + 1)).collect::<String>(),
    };
    let how = match format {
        Format::Edit => "Use the `edit` tool (the text you replace must match the file exactly, without the line-number prefix).",
        Format::EditLines => "Use the `edit_lines` tool, naming lines by the `N:hash` anchors shown in front of them.",
        Format::Write => "Use the `write` tool with the complete new content of the file.",
    };
    format!("File `{}`:\n```\n{}```\n\nTask: {}\n{how}", task.path, view, task.instruction)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FormatRow {
    pub cases: usize,
    pub first_applied: usize,
    pub succeeded: usize,
    pub first_rate: f64,
    pub success_rate: f64,
    pub meets_gate: bool,
    pub mean_calls: f64,
    /// Up to five failure messages.
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct EditsReport {
    pub formats: BTreeMap<String, FormatRow>,
    /// Best first; only formats that reached 90% within two retries.
    pub recommended: Vec<String>,
    /// Formats the session does not offer.
    pub skipped: Vec<String>,
    pub aborted: bool,
    pub provider_errors: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub mean_ms: f64,
    pub retries: usize,
}

impl EditsReport {
    /// Gate A5 holds when the best recommended format meets both rates.
    pub fn meets_a5(&self) -> bool {
        self.recommended.first().and_then(|f| self.formats.get(f)).is_some_and(|r| r.meets_gate)
    }
}

/// Formats best first: success within two retries, then first-attempt rate, then the safer
/// format (`edit`, `write`, `edit_lines`). Formats below 90% success are left out.
pub fn recommend(rows: &BTreeMap<String, FormatRow>) -> Vec<String> {
    let rank = |f: &str| match f {
        "edit" => 0,
        "write" => 1,
        _ => 2,
    };
    let mut v: Vec<(&String, &FormatRow)> = rows.iter().filter(|(_, r)| r.cases > 0 && r.success_rate >= 0.9).collect();
    v.sort_by(|a, b| {
        b.1.success_rate
            .total_cmp(&a.1.success_rate)
            .then(b.1.first_rate.total_cmp(&a.1.first_rate))
            .then(rank(a.0).cmp(&rank(b.0)))
    });
    v.into_iter().map(|(f, _)| f.clone()).collect()
}

/// Runs `tasks` through each of `formats` whose tool is in `specs`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    provider: &dyn Provider,
    model_id: &str,
    specs: &[ToolSpec],
    tasks: &[EditTask],
    formats: &[Format],
    local: bool,
    cancel: &CancelToken,
    mut progress: impl FnMut(&str, usize, usize),
) -> EditsReport {
    let gate: Gate = if local { GATE_LOCAL } else { GATE_FRONTIER };
    let mut rep = EditsReport { retries: MAX_RETRIES, ..EditsReport::default() };
    let (mut ms, mut calls) = (0.0, 0usize);
    let mut streak = 0usize;
    'formats: for &format in formats {
        let name = format_name(format);
        let Some(spec) = specs.iter().find(|s| s.name == tool_name(format)) else {
            rep.skipped.push(name.to_string());
            continue;
        };
        let mut results: Vec<CaseResult> = Vec::new();
        for (idx, task) in tasks.iter().enumerate() {
            if cancel.is_cancelled() {
                rep.aborted = true;
                break 'formats;
            }
            progress(name, idx, tasks.len());
            let case = BenchCase { id: task.id.clone(), format, original: task.original.clone(), intended: task.intended.clone(), attempts: Vec::new() };
            let first = prompt(task, format);
            let mut msgs = vec![Message::user(first)];
            let mut last: Option<marlowe_model::Completion> = None;
            let mut errored = false;
            let mut next = |k: usize, feedback: Option<&str>| -> Option<Value> {
                if k > 0 {
                    let prev = last.take()?;
                    let id = prev.tool_calls.first()?.id.clone();
                    msgs.push(prev.to_message());
                    msgs.push(Message {
                        role: Role::Tool,
                        content: vec![Part::ToolResult(ToolResult::text(id, feedback.unwrap_or("the edit failed").to_string(), true))],
                    });
                }
                let mut req = ChatRequest::new(model_id, msgs.clone());
                req.system = vec![SystemBlock { text: SYSTEM.into(), cache: false }];
                req.tools = vec![spec.clone()];
                req.max_output_tokens = Some(4096);
                let started = Instant::now();
                let done = match provider.stream(&req, cancel, &mut |_| {}) {
                    Ok(d) => d,
                    Err(e) => {
                        rep.provider_errors += 1;
                        errored = true;
                        let _ = e;
                        return None;
                    }
                };
                ms += started.elapsed().as_secs_f64() * 1000.0;
                calls += 1;
                rep.input_tokens += done.usage.input;
                rep.output_tokens += done.usage.output;
                let args = done.tool_calls.iter().find(|c| c.name == spec.name).map(|c| c.args.clone());
                last = Some(done);
                args
            };
            let result = bench::run_case(&case, &mut next);
            streak = if errored { streak + 1 } else { 0 };
            results.push(result);
            if streak >= MAX_CONSECUTIVE_ERRORS {
                rep.aborted = true;
                break;
            }
        }
        let rates = bench::score(&results).remove(&format).unwrap_or_default();
        let total_calls: usize = results.iter().map(|r| r.calls).sum();
        rep.formats.insert(
            name.to_string(),
            FormatRow {
                cases: rates.cases,
                first_applied: rates.first_applied,
                succeeded: rates.succeeded,
                first_rate: rates.first_attempt_rate(),
                success_rate: rates.success_rate(),
                meets_gate: rates.meets(gate),
                mean_calls: if rates.cases == 0 { 0.0 } else { total_calls as f64 / rates.cases as f64 },
                failures: results.iter().filter(|r| !r.succeeded).filter_map(|r| r.errors.first().map(|e| format!("{}: {e}", r.id))).take(5).collect(),
            },
        );
        if rep.aborted {
            break;
        }
    }
    rep.mean_ms = if calls == 0 { 0.0 } else { ms / calls as f64 };
    rep.recommended = recommend(&rep.formats);
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::fake::{first_user_text, Fake, Reply};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn specs() -> Vec<ToolSpec> {
        ["edit", "edit_lines", "write"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: json!({"type": "object"}) })
            .collect()
    }

    #[test]
    fn every_task_is_reachable_in_every_format_by_its_oracle() {
        let all = tasks(10_000, 1);
        assert!(all.len() >= 40, "only {} tasks", all.len());
        for t in &all {
            assert_ne!(t.original, t.intended, "{}", t.id);
            for (format, args) in &t.oracle {
                let got = bench::apply_attempt(*format, &t.original, args).unwrap_or_else(|e| panic!("{} {:?}: {e}", t.id, format));
                assert_eq!(got, t.intended, "{} {:?}", t.id, format);
            }
        }
    }

    #[test]
    fn forty_tasks_cover_every_kind_and_file_and_repeat_for_a_seed() {
        let a = tasks(40, 7);
        assert_eq!(a.len(), 40);
        let kinds: std::collections::BTreeSet<&str> = a.iter().map(|t| t.id.split('-').nth(1).unwrap()).collect();
        assert_eq!(kinds.len(), 5, "{kinds:?}");
        let files: std::collections::BTreeSet<&str> = a.iter().map(|t| t.path).collect();
        assert_eq!(files.len(), 4);
        assert_eq!(a.iter().map(|t| &t.instruction).collect::<Vec<_>>(), tasks(40, 7).iter().map(|t| &t.instruction).collect::<Vec<_>>());
        assert_ne!(a.iter().map(|t| &t.instruction).collect::<Vec<_>>(), tasks(40, 8).iter().map(|t| &t.instruction).collect::<Vec<_>>());
    }

    #[test]
    fn prompts_show_the_file_the_way_the_format_reads_it() {
        let t = &tasks(1, 1)[0];
        assert!(prompt(t, Format::Edit).contains("     1\t"));
        let anchored_view = prompt(t, Format::EditLines);
        assert!(anchored_view.contains("1:") && anchored_view.contains('|'));
        assert!(prompt(t, Format::Write).contains(&t.instruction));
    }

    /// Answers each prompt with its oracle; the first call of every third case is mangled so the
    /// retry path (and a case that never recovers) is exercised.
    fn model(mangle_every: usize, recover: bool) -> Fake {
        // Per (task, format): how many calls it has had, and which case it was when first asked.
        let seen = std::sync::Mutex::new(BTreeMap::<String, (usize, usize)>::new());
        let all = tasks(10_000, 1);
        let counter = AtomicUsize::new(0);
        Fake::new(move |req| {
            let first = first_user_text(req);
            let (task, format) = [Format::Edit, Format::EditLines, Format::Write]
                .iter()
                .find_map(|f| all.iter().find(|t| prompt(t, *f) == first).map(|t| (t, *f)))
                .expect("a known prompt");
            let key = format!("{}-{:?}", task.id, format);
            let (attempt, case_no) = {
                let mut s = seen.lock().unwrap();
                let e = s.entry(key).or_insert_with(|| (0, counter.fetch_add(1, Ordering::SeqCst)));
                e.0 += 1;
                *e
            };
            let broken = mangle_every > 0 && case_no % mangle_every == 0 && (attempt == 1 || !recover);
            let mut args = task.oracle[&format].clone();
            if broken {
                match format {
                    Format::Edit => args["old_string"] = json!("this text is not in the file"),
                    Format::EditLines => args["anchor"] = json!("1:zzz"),
                    Format::Write => args = json!({"path": task.path}),
                }
            }
            Reply::call(match format { Format::Edit => "edit", Format::EditLines => "edit_lines", Format::Write => "write" }, args)
        })
    }

    #[test]
    fn a_perfect_model_meets_the_gate_in_every_format() {
        let ts = tasks(12, 3);
        let r = run(&model(0, true), "m", &specs(), &ts, &[Format::Edit, Format::EditLines, Format::Write], true, &CancelToken::new(), |_, _, _| {});
        for f in ["edit", "edit_lines", "write"] {
            let row = &r.formats[f];
            assert_eq!((row.cases, row.first_applied, row.succeeded), (12, 12, 12), "{f}");
            assert!(row.meets_gate && row.mean_calls == 1.0);
        }
        assert_eq!(r.recommended, ["edit", "write", "edit_lines"]);
        assert!(r.meets_a5() && !r.aborted);
    }

    #[test]
    fn retries_recover_a_bad_first_call_and_the_rates_say_so() {
        let ts = tasks(12, 3);
        let r = run(&model(3, true), "m", &specs(), &ts, &[Format::Edit], true, &CancelToken::new(), |_, _, _| {});
        let row = &r.formats["edit"];
        assert_eq!(row.cases, 12);
        assert_eq!(row.first_applied, 8, "four of twelve first calls were mangled");
        assert_eq!(row.succeeded, 12, "and each recovered on the retry");
        assert!(!row.meets_gate, "92% first-attempt is not met at 67%");
        assert!(!r.meets_a5());
        assert!(row.mean_calls > 1.0);

        // A model that never recovers fails those cases for good, with the error kept.
        let r = run(&model(3, false), "m", &specs(), &ts, &[Format::Edit], true, &CancelToken::new(), |_, _, _| {});
        let row = &r.formats["edit"];
        assert_eq!(row.succeeded, 8);
        assert!(!row.failures.is_empty(), "the failing calls keep their error");
    }

    #[test]
    fn formats_the_session_does_not_offer_are_skipped_and_recommendations_rank_by_success() {
        let only_edit: Vec<ToolSpec> = specs().into_iter().filter(|s| s.name == "edit").collect();
        let r = run(&model(0, true), "m", &only_edit, &tasks(5, 1), &[Format::Edit, Format::EditLines], true, &CancelToken::new(), |_, _, _| {});
        assert_eq!(r.skipped, ["edit_lines"]);
        assert_eq!(r.recommended, ["edit"]);

        let row = |cases, first, ok| FormatRow { cases, first_applied: first, succeeded: ok, first_rate: first as f64 / cases as f64, success_rate: ok as f64 / cases as f64, ..FormatRow::default() };
        let rows = BTreeMap::from([("edit".to_string(), row(40, 30, 36)), ("edit_lines".to_string(), row(40, 39, 40)), ("write".to_string(), row(40, 40, 30))]);
        assert_eq!(recommend(&rows), ["edit_lines", "edit"], "write is below 90% and drops out");
    }

    #[test]
    fn a_dead_server_aborts_instead_of_scoring_zero_everywhere() {
        let r = run(&Fake::failing(), "m", &specs(), &tasks(20, 1), &[Format::Edit, Format::Write], true, &CancelToken::new(), |_, _, _| {});
        assert!(r.aborted);
        assert_eq!(r.provider_errors, MAX_CONSECUTIVE_ERRORS);
        assert!(!r.formats.contains_key("write"), "the second format never started");
    }
}
