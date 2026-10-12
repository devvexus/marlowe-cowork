//! Reading what a build or test run printed: which tests failed, where the compiler and linters
//! pointed, and a bounded excerpt that keeps the failures when the log is long.
//!
//! Every pattern here is specific to one tool's output, so the parsers run over all of a run's
//! text and report what matches; a tool that prints none of them yields an empty list and the
//! excerpt still carries the evidence. Failing test ids are what `build_v2` records in its
//! baseline and compares after a change ("no new failures").

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

/// One compiler, type checker or linter finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn render(&self) -> String {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let code = self.code.as_deref().map(|c| format!("[{c}]")).unwrap_or_default();
        let col = if self.col > 0 { format!(":{}", self.col) } else { String::new() };
        format!("{}:{}{col} {sev}{code}: {}", self.file, self.line, self.message)
    }
}

/// What was read from a run's output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parsed {
    /// Failing test ids (or files, for a format check), sorted and unique.
    pub failing: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    /// `3 passed, 1 failed` when the runner said so.
    pub tally: Option<String>,
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

re!(cargo_test_fail, r"(?m)^test (\S+) \.\.\. FAILED\s*$");
re!(cargo_result, r"(?m)^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed");
re!(rustc_diag, r"(?m)^(error|warning)(?:\[(\w+)\])?: ([^\n]+)\n\s*--> ([^\n]+?):(\d+):(\d+)");
re!(pytest_fail, r"(?m)^(?:FAILED|ERROR) (\S+)");
re!(pytest_tally, r"(?m)^=+ (.*\b(?:passed|failed|error|errors)\b.*?) =+\s*$");
re!(pytest_tally_q, r"(?m)^(\d+ (?:failed|passed|error)[^\n]*in [\d.]+s[^\n]*)$");
re!(go_fail, r"(?m)^\s*--- FAIL: (\S+)");
re!(go_pkg_fail, r"(?m)^FAIL\s+(\S+)\s");
re!(js_fail, r"(?m)^[ ]*(?:FAIL|✗|×|✕)[ ]+(\S.*?)\s*$");
re!(jest_bullet, r"(?m)^\s*● (\S.*?)\s*$");
re!(mocha_fail, r"(?m)^\s+\d+\) (\S.*?)\s*$");
re!(ctest_fail, r"(?m)^\s+\d+ - (\S+) \((?:Failed|Timeout|SEGFAULT|Exception|Not Run)[^)]*\)");
re!(tsc_diag, r"(?m)^(\S.*?)\((\d+),(\d+)\): (error|warning) (TS\d+): ([^\n]+)");
re!(ruff_diag, r"(?m)^(\S[^:\n]*?):(\d+):(\d+): ([A-Z]{1,4}\d{2,4}) ([^\n]+)");
re!(mypy_diag, r"(?m)^(\S[^:\n]*?):(\d+): (error|warning): ([^\n]+?)(?:  \[([\w-]+)\])?$");
re!(cc_diag, r"(?m)^(\S[^:\n]*?):(\d+):(\d+): (?:fatal )?(error|warning): ([^\n]+)");
re!(go_compile, r"(?m)^(\S+\.go):(\d+):(\d+): ([^\n]+)");
re!(important, r"(?i)\b(error|errors|failed|failure|failures|panicked|traceback|exception|assertion|assert|fatal|cannot|not found|FAIL)\b");

const MAX_FAILING: usize = 300;
const MAX_DIAGNOSTICS: usize = 40;

/// Reads `text`. `tests` enables test-failure ids (a `build` that prints `FAIL` is not a test).
pub fn parse(text: &str, tests: bool) -> Parsed {
    let mut failing = BTreeSet::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut tally = None;

    if tests {
        for c in cargo_test_fail().captures_iter(text) {
            failing.insert(c[1].to_string());
        }
        for c in pytest_fail().captures_iter(text) {
            failing.insert(c[1].trim_end_matches(':').to_string());
        }
        for c in go_fail().captures_iter(text) {
            failing.insert(c[1].to_string());
        }
        if failing.is_empty() {
            for c in go_pkg_fail().captures_iter(text) {
                failing.insert(c[1].to_string());
            }
        }
        for c in ctest_fail().captures_iter(text) {
            failing.insert(c[1].to_string());
        }
        for c in js_fail().captures_iter(text) {
            failing.insert(c[1].to_string());
        }
        for c in jest_bullet().captures_iter(text) {
            let name = c[1].to_string();
            if !name.starts_with("Test suite failed") && !name.starts_with("Console") && !name.contains("deprecated") {
                failing.insert(name);
            }
        }
        if text.contains("failing") {
            for c in mocha_fail().captures_iter(text) {
                failing.insert(c[1].to_string());
            }
        }
        let (mut passed, mut failed, mut seen) = (0u32, 0u32, false);
        for c in cargo_result().captures_iter(text) {
            seen = true;
            passed += c[1].parse::<u32>().unwrap_or(0);
            failed += c[2].parse::<u32>().unwrap_or(0);
        }
        if seen {
            tally = Some(format!("{passed} passed, {failed} failed"));
        } else if let Some(c) = pytest_tally().captures_iter(text).last().or_else(|| pytest_tally_q().captures_iter(text).last()) {
            tally = Some(c[1].trim().to_string());
        }
    }

    let num = |s: &str| s.parse::<u32>().unwrap_or(0);
    for c in rustc_diag().captures_iter(text) {
        let sev = if &c[1] == "error" { Severity::Error } else { Severity::Warning };
        add(&mut diagnostics, Diagnostic { file: c[4].to_string(), line: num(&c[5]), col: num(&c[6]), severity: sev, code: c.get(2).map(|m| m.as_str().to_string()), message: c[3].to_string() });
    }
    for c in tsc_diag().captures_iter(text) {
        let sev = if &c[4] == "error" { Severity::Error } else { Severity::Warning };
        add(&mut diagnostics, Diagnostic { file: c[1].to_string(), line: num(&c[2]), col: num(&c[3]), severity: sev, code: Some(c[5].to_string()), message: c[6].to_string() });
    }
    for c in mypy_diag().captures_iter(text) {
        let sev = if &c[3] == "error" { Severity::Error } else { Severity::Warning };
        add(&mut diagnostics, Diagnostic { file: c[1].to_string(), line: num(&c[2]), col: 0, severity: sev, code: c.get(5).map(|m| m.as_str().to_string()), message: c[4].to_string() });
    }
    for c in ruff_diag().captures_iter(text) {
        add(&mut diagnostics, Diagnostic { file: c[1].to_string(), line: num(&c[2]), col: num(&c[3]), severity: Severity::Error, code: Some(c[4].to_string()), message: c[5].to_string() });
    }
    for c in cc_diag().captures_iter(text) {
        let sev = if &c[4] == "error" { Severity::Error } else { Severity::Warning };
        add(&mut diagnostics, Diagnostic { file: c[1].to_string(), line: num(&c[2]), col: num(&c[3]), severity: sev, code: None, message: c[5].to_string() });
    }
    // Go prints `file.go:3:5: message` for compile errors, but so do mypy-less runs of other
    // tools; only trust it when nothing richer matched.
    if diagnostics.is_empty() {
        for c in go_compile().captures_iter(text) {
            add(&mut diagnostics, Diagnostic { file: c[1].to_string(), line: num(&c[2]), col: num(&c[3]), severity: Severity::Error, code: None, message: c[4].to_string() });
        }
    }
    Parsed { failing: failing.into_iter().take(MAX_FAILING).collect(), diagnostics, tally }
}

fn add(v: &mut Vec<Diagnostic>, d: Diagnostic) {
    if v.len() < MAX_DIAGNOSTICS && !v.contains(&d) {
        v.push(d);
    }
}

/// A failing-test list from a format check that lists files (`gofmt -l`).
pub fn files_listed(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).take(MAX_FAILING).collect()
}

/// At most `budget` bytes of `text`: the first lines, the last lines, and every line that looks
/// like a failure with two lines of context on each side, in order, with gaps marked. Shorter
/// text is returned whole. The boolean is whether anything was left out.
pub fn condense(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_string(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    // The start and the end of a log carry the command and the verdict; the middle is kept for
    // failure lines. Head and tail take a bounded share of the budget each.
    let mut keep: BTreeSet<usize> = BTreeSet::new();
    let mut used = 0usize;
    for (i, line) in lines.iter().enumerate().take(14) {
        if used + line.len() + 1 > budget / 6 {
            break;
        }
        used += line.len() + 1;
        keep.insert(i);
    }
    let mut tail_used = 0usize;
    for i in (0..n).rev().take(70) {
        if tail_used + lines[i].len() + 1 > budget * 2 / 5 {
            break;
        }
        tail_used += lines[i].len() + 1;
        keep.insert(i);
    }
    used += tail_used;
    let limit = budget.saturating_sub(200);
    'outer: for i in 0..n {
        if !important().is_match(lines[i]) {
            continue;
        }
        for j in i.saturating_sub(2)..(i + 3).min(n) {
            if keep.contains(&j) {
                continue;
            }
            let cost = lines[j].len() + 1;
            if used + cost > limit {
                break 'outer;
            }
            used += cost;
            keep.insert(j);
        }
    }
    let mut out = String::with_capacity(used + 128);
    let mut prev: Option<usize> = None;
    for &i in &keep {
        match prev {
            Some(p) if i > p + 1 => out.push_str(&format!("... [{} lines omitted] ...\n", i - p - 1)),
            None if i > 0 => out.push_str(&format!("... [{i} lines omitted] ...\n")),
            _ => {}
        }
        out.push_str(&clip_line(lines[i], 600));
        out.push('\n');
        prev = Some(i);
    }
    (out.trim_end().to_string(), true)
}

fn clip_line(l: &str, max: usize) -> String {
    if l.len() <= max {
        return l.to_string();
    }
    let mut cut = max;
    while !l.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... [+{} bytes]", &l[..cut], l.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_failures_and_compile_errors() {
        let log = "running 3 tests\ntest a::ok ... ok\ntest parser::tests::trailing ... FAILED\ntest b::bad ... FAILED\n\nfailures:\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored\n\nerror[E0425]: cannot find value `x` in this scope\n  --> src/main.rs:3:5\n   |\n";
        let p = parse(log, true);
        assert_eq!(p.failing, ["b::bad", "parser::tests::trailing"]);
        assert_eq!(p.tally.as_deref(), Some("1 passed, 2 failed"));
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].render(), "src/main.rs:3:5 error[E0425]: cannot find value `x` in this scope");
        assert!(parse(log, false).failing.is_empty(), "a build's output has no test ids");
    }

    #[test]
    fn pytest_go_ctest_and_js() {
        let py = "FAILED tests/test_x.py::test_login - assert 1 == 2\nERROR tests/test_y.py::test_boot\n=== 1 failed, 4 passed in 0.31s ===\n";
        let p = parse(py, true);
        assert_eq!(p.failing, ["tests/test_x.py::test_login", "tests/test_y.py::test_boot"]);
        assert_eq!(p.tally.as_deref(), Some("1 failed, 4 passed in 0.31s"));
        let go = "--- FAIL: TestSum (0.00s)\n    sum_test.go:9: got 3\nFAIL\nFAIL\texample.com/m\t0.002s\n";
        assert_eq!(parse(go, true).failing, ["TestSum"]);
        let ct = "The following tests FAILED:\n\t  2 - widget_test (Failed)\n\t  5 - net_test (Timeout)\n";
        assert_eq!(parse(ct, true).failing, ["net_test", "widget_test"]);
        let js = " FAIL  src/a.test.ts\n  ● math › adds\n\n    expect(received).toBe(expected)\n";
        let p = parse(js, true);
        assert!(p.failing.contains(&"src/a.test.ts".to_string()) && p.failing.contains(&"math › adds".to_string()), "{:?}", p.failing);
    }

    #[test]
    fn linters_and_type_checkers() {
        let log = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                   pkg/mod.py:7: error: Incompatible return value type  [return-value]\n\
                   src/x.py:3:1: F401 `os` imported but unused\n\
                   main.c:10:3: error: expected ';' before '}' token\n";
        let p = parse(log, false);
        let r: Vec<String> = p.diagnostics.iter().map(Diagnostic::render).collect();
        assert!(r.iter().any(|d| d == "src/app.ts:12:5 error[TS2322]: Type 'string' is not assignable to type 'number'."), "{r:?}");
        assert!(r.iter().any(|d| d == "pkg/mod.py:7 error[return-value]: Incompatible return value type"), "{r:?}");
        assert!(r.iter().any(|d| d.starts_with("src/x.py:3:1 error[F401]")), "{r:?}");
        assert!(r.iter().any(|d| d.starts_with("main.c:10:3 error:")), "{r:?}");
    }

    #[test]
    fn condense_keeps_failures_from_the_middle_of_a_long_log() {
        let mut log = String::new();
        for i in 0..4000 {
            log.push_str(&format!("compiling crate number {i} of a very large dependency tree\n"));
            if i == 2000 {
                log.push_str("error[E0308]: mismatched types\n  --> src/lib.rs:9:1\n");
            }
        }
        let (out, cut) = condense(&log, 4000);
        assert!(cut && out.len() < 6000, "{}", out.len());
        assert!(out.contains("error[E0308]: mismatched types") && out.contains("src/lib.rs:9:1"));
        assert!(out.contains("compiling crate number 0 ") && out.contains("compiling crate number 3999 "));
        assert!(out.contains("lines omitted"));
        assert_eq!(condense("short\n", 4000), ("short\n".to_string(), false));
    }
}
