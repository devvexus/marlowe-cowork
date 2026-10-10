//! build: plan (Engineer) → implement in the worktree (Engineer) → test loop (the repo's test
//! command, up to N fix-and-retest iterations) → review (Reviewer reads the diff and lists
//! blocking issues; the Engineer fixes once, then tests again) → Dispatch with a DiffSummary.
//! The run then waits in `NeedsYou` for the user's merge decision.

use std::path::Path;

use marlowe_proto::{Confidence, Dispatch, SandboxKind};

use super::research::first_paragraph;
use super::{PlaybookConfig, ShellResult, StepRunner, StepSpec};
use crate::playbook::agent::tail_chars;
use crate::CrewError;

/// Read-only tools for the plan step.
const LOOK: [&str; 3] = ["read", "glob", "grep"];

/// What the reviewer may use on a diff: read the tree and run the tests, nothing else.
const REVIEW_TOOLS: [&str; 5] = ["read", "glob", "grep", "bash", "powershell"];

/// Characters of diff the reviewer is shown inline; the rest it can read in the worktree.
const DIFF_CHARS: usize = 60_000;

/// Characters of failing test output handed to the engineer.
const FAIL_CHARS: usize = 8_000;

/// Where the tests stand.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tests {
    /// No test command was given or detected.
    NotRun,
    Green { fixes: u32 },
    Red { fixes: u32, output: String },
}

/// Runs the build playbook for `goal`.
pub fn build(runner: &dyn StepRunner, goal: &str, cfg: &PlaybookConfig) -> Result<Dispatch, CrewError> {
    let worktree = runner.sandbox().kind == SandboxKind::Worktree;
    let root = runner.sandbox().root.clone();
    let command = cfg.test_command.clone().or_else(|| detect_test_command(&root));

    let plan = runner.step(StepSpec {
        key: "plan".into(),
        role: "engineer".into(),
        label: "plan".into(),
        prompt: format!(
            "Goal: {goal}\n\nPlan the change. Read the code it touches first, then reply with the plan \
             in a few lines: what changes where, and how you will know it works."
        ),
        tools: Some(LOOK.iter().map(|t| t.to_string()).collect()),
        effort: None,
    })?;

    let test_line = match &command {
        Some(c) => format!("The test command is `{c}`; it runs after you finish, and failures come back to you."),
        None => "No test command was found for this checkout; say exactly how you verified the change.".into(),
    };
    let mut engineer = runner
        .step(StepSpec {
            key: "implement".into(),
            role: "engineer".into(),
            label: "implement".into(),
            prompt: format!(
                "Goal: {goal}\n\nYour plan:\n{}\n\nImplement it in your worktree. {test_line} Finish with \
                 what changed and why, then how you verified it.",
                plan.text.trim()
            ),
            tools: None,
            effort: None,
        })?
        .text;

    let mut tests = Tests::NotRun;
    if let Some(cmd) = &command {
        let (t, last) = test_loop(runner, goal, cmd, cfg.max_test_iterations)?;
        tests = t;
        if let Some(text) = last {
            engineer = text;
        }
    }

    // Review the diff; the engineer answers blocking issues once, then the tests run again.
    let diff_text = if worktree { runner.sandbox().diff_text()? } else { String::new() };
    let mut review_note = "no changes to review".to_string();
    let mut blocking = false;
    if !worktree || !diff_text.trim().is_empty() {
        let shown = if worktree {
            let cut: String = diff_text.chars().take(DIFF_CHARS).collect();
            let more = if cut.len() < diff_text.len() { "\n[… diff cut; read the rest in the worktree]" } else { "" };
            format!("The diff against the base commit:\n```diff\n{cut}\n```{more}")
        } else {
            format!("The change is in {}.", root.display())
        };
        let review = runner.step(StepSpec {
            key: "review".into(),
            role: "reviewer".into(),
            label: "review".into(),
            prompt: format!(
                "Goal: {goal}\n\nTests: {}.\n\n{shown}\n\nReview the change against the goal. List blocking \
                 issues first, each with file, line and the input that breaks it. If nothing blocks, say \
                 `No blocking issues.`",
                tests_line(&tests, command.as_deref())
            ),
            tools: Some(REVIEW_TOOLS.iter().map(|t| t.to_string()).collect()),
            effort: None,
        })?;
        blocking = !review.text.to_ascii_lowercase().contains("no blocking issues");
        review_note = if blocking {
            "the reviewer raised blocking issues; the engineer answered them once".to_string()
        } else {
            "no blocking issues".to_string()
        };
        if blocking {
            engineer = runner
                .step(StepSpec {
                    key: "fix".into(),
                    role: "engineer".into(),
                    label: "fix review".into(),
                    prompt: format!(
                        "Goal: {goal}\n\nThe reviewer's findings:\n{}\n\nFix each blocking issue in your worktree, \
                         or say why it is not one. Then say what you changed.",
                        review.text.trim()
                    ),
                    tools: None,
                    effort: None,
                })?
                .text;
            if let Some(cmd) = &command {
                runner.progress("test after review");
                let fixes = match &tests {
                    Tests::Green { fixes } | Tests::Red { fixes, .. } => *fixes,
                    Tests::NotRun => 0,
                };
                let r = runner.shell(cmd)?;
                tests = if r.ok { Tests::Green { fixes } } else { Tests::Red { fixes, output: r.output } };
            }
        }
    }

    let diff = if worktree { Some(runner.sandbox().diff_summary()?) } else { None };
    let changed = diff.as_ref().map(|d| d.files.len());
    let confidence = match (&tests, blocking, changed) {
        (_, _, Some(0)) => Confidence::Low,
        (Tests::Red { .. }, _, _) => Confidence::Low,
        (Tests::Green { .. }, false, _) => Confidence::High,
        (Tests::Green { .. }, true, _) | (Tests::NotRun, false, _) => Confidence::Medium,
        (Tests::NotRun, true, _) => Confidence::Low,
    };

    let mut summary = first_paragraph(&engineer, 900);
    summary.push_str(&format!("\n\nTests: {}.", tests_line(&tests, command.as_deref())));
    summary.push_str(&format!(" Review: {review_note}."));
    if let Some(d) = &diff {
        if d.files.is_empty() {
            summary.push_str(" The worktree has no changes, so there is nothing to merge.");
        } else {
            summary.push_str(&format!(
                " {} file{} changed, +{} −{} on `{}`; it needs a merge decision.",
                d.files.len(),
                if d.files.len() == 1 { "" } else { "s" },
                d.insertions,
                d.deletions,
                d.branch
            ));
        }
    }
    if let Tests::Red { output, .. } = &tests {
        summary.push_str(&format!("\n\nLast failing output:\n```\n{}\n```", tail_lines(output, 15)));
    }
    let artifacts = match &diff {
        Some(d) => d.files.iter().map(|f| root.join(f).display().to_string()).collect(),
        None => vec![root.display().to_string()],
    };
    Ok(Dispatch { summary, artifacts, confidence, citations: Vec::new(), diff })
}

/// Runs the tests, handing each failure to the engineer, up to `max_fixes` times. Returns the
/// state and the engineer's last reply when it made one. A resumed run starts after the fix
/// rounds it already completed (`test:1`, `test:2`, ...), so the count and the reply stay true.
fn test_loop(
    runner: &dyn StepRunner,
    goal: &str,
    cmd: &str,
    max_fixes: u32,
) -> Result<(Tests, Option<String>), CrewError> {
    let mut last = None;
    let mut fixes = 0;
    while let Some(done) = runner.completed(&format!("test:{}", fixes + 1)) {
        fixes += 1;
        last = Some(done.text);
    }
    loop {
        runner.progress(&format!("test {}", fixes + 1));
        let r = runner.shell(cmd)?;
        if r.ok {
            return Ok((Tests::Green { fixes }, last));
        }
        if fixes >= max_fixes {
            return Ok((Tests::Red { fixes, output: r.output }, last));
        }
        fixes += 1;
        let text = runner
            .step(StepSpec {
                key: format!("test:{fixes}"),
                role: "engineer".into(),
                label: format!("fix tests {fixes}/{max_fixes}"),
                prompt: fix_prompt(goal, cmd, &r),
                tools: None,
                effort: None,
            })?
            .text;
        last = Some(text);
    }
}

fn fix_prompt(goal: &str, cmd: &str, r: &ShellResult) -> String {
    let code = r.exit_code.map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into());
    format!(
        "Goal: {goal}\n\n`{cmd}` failed ({code}):\n```\n{}\n```\n\nRead the failure, fix the cause in your \
         worktree, and say what you changed.",
        tail_chars(&r.output, FAIL_CHARS)
    )
}

fn tests_line(t: &Tests, cmd: Option<&str>) -> String {
    let cmd = cmd.unwrap_or("tests");
    match t {
        Tests::NotRun => "not run (no test command found)".into(),
        Tests::Green { fixes: 0 } => format!("`{cmd}` green on the first run"),
        Tests::Green { fixes } => format!("`{cmd}` green after {fixes} fix round{}", if *fixes == 1 { "" } else { "s" }),
        Tests::Red { fixes, .. } => format!("`{cmd}` still failing after {fixes} fix round{}", if *fixes == 1 { "" } else { "s" }),
    }
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The test command for a checkout, from its manifest: `cargo test` (Cargo.toml),
/// `npm test` (package.json with a `test` script), `python -m pytest -q` (pyproject.toml,
/// pytest.ini, setup.cfg or tox.ini), `go test ./...` (go.mod). `None` when nothing matches.
pub fn detect_test_command(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        return Some("cargo test".into());
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        let has_test = serde_json::from_str::<serde_json::Value>(&pkg)
            .ok()
            .and_then(|v| v.get("scripts")?.get("test")?.as_str().map(|s| !s.trim().is_empty()))
            .unwrap_or(false);
        if has_test {
            return Some("npm test".into());
        }
    }
    if ["pyproject.toml", "pytest.ini", "setup.cfg", "tox.ini"].iter().any(|f| root.join(f).is_file()) {
        return Some("python -m pytest -q".into());
    }
    if root.join("go.mod").is_file() {
        return Some("go test ./...".into());
    }
    None
}
