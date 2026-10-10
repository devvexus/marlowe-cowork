//! Playbooks: deterministic Rust orchestration where each step is one agent call with a role.
//! Steps pass prose to each other; the few things a playbook needs to pick out of that prose
//! (a numbered list, markdown links, a `Confidence:` line) are read leniently here.

mod agent;
mod build;
mod research;
pub mod research_v2;

pub use agent::{
    sandbox_shell, tail_chars, AgentRunner, ModelResolver, NoHooks, ShellFn, StepHooks, StepModel, CONTINUE_PROMPT,
    MAX_CONTINUES, SHELL_OUTPUT_CHARS, SHELL_TIMEOUT,
};
pub use build::{build, detect_test_command};
pub use research::{deep_research, first_paragraph, min_confidence, parse_verdicts, Verdict, REPORT_FILE};
pub use research_v2::{deep_research_v2, ResearchHost, ResearchIo, ResearchV2Config};

use marlowe_model::{Effort, Usage};
use marlowe_proto::{Citation, Confidence, Dispatch, Playbook, SandboxKind};

use crate::sandbox::Sandbox;
use crate::CrewError;

/// One agent call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpec {
    /// Checkpoint key: `plan`, `scout:2`, `verify`, `synthesize`, `implement`, `test:1`,
    /// `review`, `fix`, `main`. A resumed run returns the stored output for a completed key.
    pub key: String,
    pub role: String,
    /// Shown as the run's current action: `scout 2/4`.
    pub label: String,
    /// The step's user message: the goal plus what earlier steps produced.
    pub prompt: String,
    /// Narrows the role's allowlist for this step; `None` keeps it.
    pub tools: Option<Vec<String>>,
    /// Overrides the card's effort for this step; `None` keeps it.
    pub effort: Option<Effort>,
}

/// What one agent call produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepResult {
    /// The final assistant text.
    pub text: String,
    pub usage: Usage,
    /// The step read untrusted content.
    pub tainted: bool,
}

/// A shell command's result in the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    /// Combined, bounded output.
    pub output: String,
}

/// What a playbook drives. The supervisor's run thread implements it; checkpoints, budget,
/// steer inbox, cancel, pause and approvals live behind it, so playbooks stay straight-line code.
pub trait StepRunner: Send + Sync {
    /// Runs one agent call to completion (or returns the stored output of a completed key).
    fn step(&self, spec: StepSpec) -> Result<StepResult, CrewError>;

    /// The stored output of `key` when a resumed run already completed it, without running it.
    fn completed(&self, _key: &str) -> Option<StepResult> {
        None
    }

    /// Runs independent agent calls concurrently; results in input order.
    fn parallel(&self, specs: Vec<StepSpec>) -> Vec<Result<StepResult, CrewError>>;

    /// Runs a command in the sandbox (bounded, in a Job Object, crew environment).
    fn shell(&self, command: &str) -> Result<ShellResult, CrewError>;

    /// The run's sandbox.
    fn sandbox(&self) -> &Sandbox;

    /// Sets the run's one-line current action.
    fn progress(&self, action: &str);
}

/// Playbook tunables (from `CrewConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookConfig {
    pub max_scouts: usize,
    pub max_test_iterations: u32,
    /// The build test command; `None` detects one from the worktree.
    pub test_command: Option<String>,
    /// Deep research v2 settings.
    pub research: ResearchV2Config,
}

impl Default for PlaybookConfig {
    fn default() -> Self {
        Self { max_scouts: 6, max_test_iterations: 4, test_command: None, research: ResearchV2Config::default() }
    }
}

/// Runs `playbook` for `goal`, or a single role-prompt step with `role` when there is none.
/// Deep research is v1 here; [`run_with`] runs v2 when the run has research channels.
pub fn run(
    playbook: Option<Playbook>,
    role: &str,
    goal: &str,
    runner: &dyn StepRunner,
    cfg: &PlaybookConfig,
) -> Result<Dispatch, CrewError> {
    run_with(playbook, role, goal, runner, cfg, None)
}

/// [`run`], with the research host that makes deep research v2. Without one (no channels wired)
/// deep research falls back to v1, whose scouts search with their own tools.
pub fn run_with(
    playbook: Option<Playbook>,
    role: &str,
    goal: &str,
    runner: &dyn StepRunner,
    cfg: &PlaybookConfig,
    research: Option<&dyn ResearchHost>,
) -> Result<Dispatch, CrewError> {
    match (playbook, research) {
        (Some(Playbook::DeepResearch), Some(host)) => deep_research_v2(runner, host, goal, &cfg.research),
        (Some(Playbook::DeepResearch), None) => deep_research(runner, goal, cfg),
        (Some(Playbook::Build), _) => build(runner, goal, cfg),
        (None, _) => single(runner, role, goal),
    }
}

/// A role-prompt dispatch (Investigate, Review, Experiment): one step, its text is the summary.
/// A worktree run's diff comes along when it changed anything.
pub fn single(runner: &dyn StepRunner, role: &str, goal: &str) -> Result<Dispatch, CrewError> {
    let r = runner.step(StepSpec {
        key: "main".into(),
        role: role.to_string(),
        label: role.to_string(),
        prompt: format!("Goal: {goal}"),
        tools: None,
        effort: None,
    })?;
    let diff = if runner.sandbox().kind == SandboxKind::Worktree {
        Some(runner.sandbox().diff_summary()?).filter(|d| !d.files.is_empty())
    } else {
        None
    };
    let artifacts = diff
        .as_ref()
        .map(|d| d.files.iter().map(|f| runner.sandbox().root.join(f).display().to_string()).collect())
        .unwrap_or_default();
    Ok(Dispatch {
        summary: r.text.trim().to_string(),
        artifacts,
        confidence: confidence_line(&r.text).unwrap_or(Confidence::Medium),
        citations: markdown_links(&r.text),
        diff,
    })
}

/// Items of a numbered (`1.` / `1)`) or bulleted (`-` / `*`) list, in order, markers stripped.
pub fn list_items(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim_start();
            let rest = if let Some(r) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                r
            } else {
                let digits = t.chars().take_while(char::is_ascii_digit).count();
                if digits == 0 {
                    return None;
                }
                t[digits..].strip_prefix(". ").or_else(|| t[digits..].strip_prefix(") "))?
            };
            let rest = rest.trim();
            (!rest.is_empty()).then(|| rest.to_string())
        })
        .collect()
}

/// Markdown links `[title](target)`, in order, deduplicated by target.
pub fn markdown_links(text: &str) -> Vec<Citation> {
    let mut out: Vec<Citation> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let title = &after[..close];
        let tail = &after[close + 2..];
        let Some(end) = tail.find(')') else { break };
        let target = tail[..end].trim();
        if !title.contains(['[', ']']) && !target.is_empty() && !target.contains(char::is_whitespace) {
            if !out.iter().any(|c| c.url_or_path == target) {
                out.push(Citation { title: title.trim().to_string(), url_or_path: target.to_string() });
            }
            rest = &tail[end + 1..];
        } else {
            rest = after;
        }
    }
    out
}

/// The last `Confidence: low|medium|high` line, if any.
pub fn confidence_line(text: &str) -> Option<Confidence> {
    text.lines().rev().find_map(|l| {
        let t = l.trim().trim_matches('*').trim();
        let (k, v) = t.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case("confidence") {
            return None;
        }
        match v.trim().trim_matches(|c: char| c == '*' || c == '.').to_ascii_lowercase().as_str() {
            "low" => Some(Confidence::Low),
            "medium" => Some(Confidence::Medium),
            "high" => Some(Confidence::High),
            _ => None,
        }
    })
}

/// A file-name slug: lower-case ASCII words joined by `-`, at most 60 chars, never empty.
pub fn slug(text: &str) -> String {
    let mut s = String::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        if s.len() + word.len() + 1 > 60 {
            break;
        }
        if !s.is_empty() {
            s.push('-');
        }
        s.push_str(&word.to_ascii_lowercase());
    }
    if s.is_empty() {
        "research".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests;
