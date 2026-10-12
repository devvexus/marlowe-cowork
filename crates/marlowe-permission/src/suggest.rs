//! Rule suggestions for `/fewer-permission-prompts` (M14): approved read-only calls in a project's
//! transcripts, ranked, never a tripwire or fixed-approval pattern.
//!
//! The caller (the daemon) lists the calls a project's past sessions ran ([`Observed`]: the shell
//! lines and MCP tools that were approved and ran) and the rules already in force. A shell line
//! counts only when this crate's own analysis says every command in it only reads
//! ([`crate::shell::Analysis::read_only`]) and nothing in it trips a hard deny, a tripwire or a new
//! host: a push, a delete outside the workspace, an unparseable line never becomes a suggestion,
//! and a command that builds or runs code (`cargo test`) is not a read. A suggestion is as narrow
//! as is safe: `git status:*` for the git subcommands with no flag that runs or writes anything,
//! `ls:*` for programs whose every use reads, the exact line for anything else (`rg --pre`, `git
//! log --output=f` and `git diff --ext-diff` run or write through a flag, and a prefix rule covers
//! any arguments, so those programs are suggested only as the exact line observed). An MCP tool counts when its name says it only
//! looks (`get_`, `list_`, `search_` ...). Calls an existing allow rule already covers are skipped.
//!
//! Wire shape: `marlowe_proto::RuleSuggestion` (the daemon converts).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::rules::{Action, Kind, RuleSet, Subject};
use crate::shell::{analyze, Dialect, Simple, ShellCtx};

/// The fewest times a pattern must have been approved before it is suggested.
pub const MIN_COUNT: u32 = 3;
/// The most suggestions returned.
pub const MAX_SUGGESTIONS: usize = 25;

/// One call a past session ran after it was approved (or ran unasked and was not refused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// `bash`, `powershell` or an MCP tool's full name (`mcp__github__get_issue`).
    pub tool: String,
    /// The shell line, for a shell tool.
    pub command: Option<String>,
}

/// One suggested allow rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// `bash(git status:*)`.
    pub pattern: String,
    /// Observed calls the pattern would have covered.
    pub count: u32,
    /// One call it covers.
    pub example: String,
}

/// Programs with no flag that runs another program or writes a file: a prefix rule for one of them
/// cannot be turned into an exec or a write by its arguments. Left out on purpose: `rg` (`--pre`,
/// `--hostname-bin`), `date` (`-s`), `hostname` (sets it), `sort -o`, `uniq in out`, `tree -o`,
/// `file -C`.
const READ_PROGRAMS: &[&str] = &[
    "ls", "dir", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "pwd", "which", "where", "type", "stat", "diff",
    "cut", "basename", "dirname", "realpath", "whoami", "get-childitem", "get-content", "select-string", "get-location",
    "test-path", "measure-object",
];

/// Git subcommands that take no flag which runs or writes anything. `log`, `diff`, `show` and `grep`
/// are not here (`--output=`, `--ext-diff`, `--textconv`, `-O<cmd>`), nor are `blame`, `cat-file`,
/// `rev-list` and `shortlog`, which share those option sets.
const GIT_READ: &[&str] = &["status", "ls-files", "ls-tree", "rev-parse", "describe"];

/// Words in an MCP tool's name that mean it only looks.
const MCP_READ_PREFIXES: &[&str] = &["get_", "list_", "read_", "search_", "find_", "describe_", "fetch_", "query_", "view_", "show_"];

/// The pattern for one command of an all-read-only line, or `None` when it cannot be generalised
/// safely (a program reached by path, dynamic words).
fn pattern_for(c: &Simple) -> Option<String> {
    if c.dynamic || c.opaque || c.writes_redirect || c.typed_line.is_some() || c.dangerous.is_some() {
        return None;
    }
    let program = c.program.as_str();
    if READ_PROGRAMS.contains(&program) {
        return Some(format!("{program}:*"));
    }
    if program == "git" {
        // Only the bare subcommand: `git -c core.pager=x status` is another command.
        let sub = c.args.first().map(String::as_str).unwrap_or("");
        if GIT_READ.contains(&sub) {
            return Some(format!("git {sub}:*"));
        }
    }
    // Anything else that only reads, on this exact line: it repeats or it is not suggested.
    Some(c.line.clone())
}

/// Whether an MCP tool's name says it only reads.
pub fn mcp_reads(tool: &str) -> bool {
    tool.strip_prefix("mcp__")
        .and_then(|r| r.split_once("__"))
        .is_some_and(|(_, name)| MCP_READ_PREFIXES.iter().any(|p| name.starts_with(p)))
}

/// Ranks suggestions from `calls`. `roots` are the project's writable roots (what the analysis
/// treats as inside), `existing` the rules already in force.
pub fn suggest(calls: &[Observed], roots: &[PathBuf], existing: &RuleSet) -> Vec<Suggestion> {
    let cwd = roots.first().cloned().unwrap_or_default();
    let mut counts: HashMap<String, (u32, String)> = HashMap::new();
    for call in calls {
        if let Some(name) = call.tool.strip_prefix("mcp__").map(|_| call.tool.as_str()) {
            if !mcp_reads(name) {
                continue;
            }
            let mut subject = Subject::new(name, Kind::Mcp);
            subject.root = roots.first().map(PathBuf::as_path);
            if existing.evaluate(&subject).is_some_and(|m| m.action == Action::Allow) {
                continue;
            }
            let e = counts.entry(name.to_string()).or_insert((0, name.to_string()));
            e.0 += 1;
            continue;
        }
        let (Some(dialect), Some(line)) = (Dialect::of_tool(&call.tool), call.command.as_deref()) else { continue };
        let ctx = ShellCtx::workspace(cwd.clone(), roots);
        let a = analyze(dialect, line, &ctx);
        // A tripwire, a deny, a new host or a line that could not be read is never suggested; a
        // line that does more than read is not a read.
        if !a.read_only() {
            continue;
        }
        let tool = if dialect == Dialect::PowerShell { "powershell" } else { "bash" };
        let mut subject = Subject::new(tool, Kind::Shell);
        subject.shell = Some(&a);
        subject.root = roots.first().map(PathBuf::as_path);
        if existing.evaluate(&subject).is_some_and(|m| m.action == Action::Allow) {
            continue;
        }
        let mut patterns: Vec<String> = Vec::new();
        for c in a.rule_commands() {
            match pattern_for(c) {
                Some(p) => {
                    let p = format!("{tool}({p})");
                    if !patterns.contains(&p) {
                        patterns.push(p);
                    }
                }
                None => {
                    patterns.clear();
                    break;
                }
            }
        }
        for p in patterns {
            let e = counts.entry(p).or_insert((0, line.lines().next().unwrap_or("").chars().take(120).collect()));
            e.0 += 1;
        }
    }
    let mut out: Vec<Suggestion> = counts
        .into_iter()
        .filter(|(_, (n, _))| *n >= MIN_COUNT)
        .map(|(pattern, (count, example))| Suggestion { pattern, count, example })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.pattern.cmp(&b.pattern)));
    out.truncate(MAX_SUGGESTIONS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\ws\proj" } else { "/ws/proj" })
    }

    fn sh(line: &str, n: usize) -> Vec<Observed> {
        (0..n).map(|_| Observed { tool: "bash".into(), command: Some(line.into()) }).collect()
    }

    fn run(calls: &[Observed], existing: &RuleSet) -> Vec<Suggestion> {
        suggest(calls, &[root()], existing)
    }

    #[test]
    fn parity_m14_proposes_the_read_only_patterns_and_never_a_push_or_a_build() {
        // The matrix fixture: 5x git status, 4x rg (each with its own query), 3x cargo test, 1x
        // git push. `rg` takes `--pre <cmd>`, so a prefix rule for it would let the program run
        // anything unasked: it is suggested only as an exact line, and four different lines are
        // four singletons.
        let mut calls = sh("git status", 5);
        for q in ["fn main", "TODO", "unsafe", "struct Agent"] {
            calls.extend(sh(&format!("rg \"{q}\" src"), 1));
        }
        calls.extend(sh("cargo test -p marlowe-agent", 3));
        calls.extend(sh("git push origin feature", 1));
        let got = run(&calls, &RuleSet::new());
        let patterns: Vec<&str> = got.iter().map(|s| s.pattern.as_str()).collect();
        assert_eq!(patterns, ["bash(git status:*)"], "{got:?}");
        assert_eq!(got[0].count, 5);
        assert_eq!(got[0].example, "git status");
        // The suggestions are rules the engine parses and that cover the calls they came from.
        let mut set = RuleSet::new();
        for s in &got {
            set.add_text(Action::Allow, &s.pattern).unwrap();
        }
        assert!(run(&calls, &set).is_empty(), "once allowed they are no longer suggested");
    }

    #[test]
    fn a_count_below_the_minimum_is_not_suggested() {
        assert!(run(&sh("git status -s", MIN_COUNT as usize - 1), &RuleSet::new()).is_empty());
        assert_eq!(run(&sh("git status -s", MIN_COUNT as usize), &RuleSet::new())[0].pattern, "bash(git status:*)");
    }

    #[test]
    fn writes_deletes_and_unparseable_lines_are_never_suggested() {
        for line in [
            "git push --force origin main",
            "git commit -m x",
            "rm -rf target",
            "cat a > b",
            "echo $(curl http://example.test)",
            "echo \"never closed",
            "npm install left-pad",
            "git branch -D old",
        ] {
            assert!(run(&sh(line, 6), &RuleSet::new()).is_empty(), "{line}");
        }
    }

    #[test]
    fn a_line_of_several_reads_counts_each_pattern_once() {
        let got = run(&sh("git status && git ls-files", 3), &RuleSet::new());
        let patterns: Vec<&str> = got.iter().map(|s| s.pattern.as_str()).collect();
        assert_eq!(patterns, ["bash(git ls-files:*)", "bash(git status:*)"]);
    }

    /// Whether `line` would run with no prompt under `set`.
    fn allowed(set: &RuleSet, line: &str) -> bool {
        let roots = [root()];
        let ctx = ShellCtx::workspace(root(), &roots);
        let a = analyze(Dialect::Bash, line, &ctx);
        let mut subject = Subject::new("bash", Kind::Shell);
        subject.shell = Some(&a);
        subject.root = Some(roots[0].as_path());
        set.evaluate(&subject).is_some_and(|m| m.action == Action::Allow)
    }

    #[test]
    fn accepting_every_suggestion_never_frees_a_call_that_runs_or_writes_through_a_flag() {
        // Benign uses of programs whose flags can exec or write, each seen often enough to suggest.
        let mut calls = Vec::new();
        for line in [
            "rg TODO src", "git log --oneline", "git diff --stat", "git show HEAD", "git grep fn", "date", "hostname",
            "git status", "ls -la", "grep -rn fn src", "sort a.txt", "git blame src/main.rs",
        ] {
            calls.extend(sh(line, 4));
        }
        let mut set = RuleSet::new();
        for s in run(&calls, &RuleSet::new()) {
            set.add_text(Action::Allow, &s.pattern).unwrap();
        }
        assert!(allowed(&set, "git status") && allowed(&set, "ls -la"), "the safe prefixes were accepted");
        // Hostile fixtures built from pieces so no scanner mistakes this file for the payload.
        let pre = ["rg --", "pre ", "./x.sh TODO src"].concat();
        let out = ["git log --", "output=f"].concat();
        let ext = ["git diff --ext", "-diff"].concat();
        let grep_o = ["git grep -O", "sh fn"].concat();
        let set_date = ["date -", "s 2020-01-01"].concat();
        let sort_o = ["sort -", "o out.txt a.txt"].concat();
        for bad in [pre, out, ext, grep_o, set_date, sort_o] {
            assert!(!allowed(&set, &bad), "{bad} must still ask");
        }
    }

    #[test]
    fn an_unknown_read_only_line_is_suggested_exactly_or_not_at_all() {
        // `terraform --version` is not a known reader but the analysis may still call it a read;
        // whatever it does, a suggestion for it is the exact line, never `terraform:*`.
        for s in run(&sh("terraform --version", 4), &RuleSet::new()) {
            assert!(!s.pattern.contains(":*"), "{s:?}");
        }
    }

    #[test]
    fn mcp_tools_that_only_look_are_suggested_by_name() {
        let call = |t: &str, n: usize| (0..n).map(|_| Observed { tool: t.into(), command: None }).collect::<Vec<_>>();
        let mut calls = call("mcp__github__get_issue", 4);
        calls.extend(call("mcp__github__delete_repo", 9));
        calls.extend(call("mcp__github__list_prs", 2));
        let got = run(&calls, &RuleSet::new());
        assert_eq!(got.iter().map(|s| s.pattern.as_str()).collect::<Vec<_>>(), ["mcp__github__get_issue"]);
        let mut set = RuleSet::new();
        set.add_text(Action::Allow, "mcp__github").unwrap();
        assert!(run(&calls, &set).is_empty(), "a server-wide allow already covers it");
    }
}
