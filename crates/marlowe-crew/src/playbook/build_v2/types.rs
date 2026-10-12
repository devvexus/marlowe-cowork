//! build_v2's data: the plan the lead proposes, the baseline the gate compares against, review
//! findings, and what became of each task. Everything here is checkpointed as JSON, so a resumed
//! run reads back exactly what it decided.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// One task of the lead's DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Short and unique: `t1`, `parser`.
    pub id: String,
    pub goal: String,
    /// Files the task may change (relative paths). Overlap outside the hub list serialises.
    #[serde(default)]
    pub files_owned: Vec<String>,
    /// What other tasks may rely on: signatures, types, routes.
    #[serde(default)]
    pub interfaces: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The task's acceptance test command (the contract stage writes the test, failing).
    #[serde(default)]
    pub accept_cmd: Option<String>,
}

/// The lead's plan, validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    pub tasks: Vec<Task>,
    /// Files every task may need (manifests, registries, module declarations): the contract
    /// stage edits them, then they are frozen.
    #[serde(default)]
    pub hubs: Vec<String>,
    /// What the lead had to change to make the plan runnable (serialised overlaps), for the user.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl BuildPlan {
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Tasks grouped into waves: each wave's tasks depend only on earlier waves. Plan order
    /// within a wave (the merge order).
    pub fn waves(&self) -> Vec<Vec<String>> {
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut left: Vec<&Task> = self.tasks.iter().collect();
        let mut waves = Vec::new();
        while !left.is_empty() {
            let (ready, rest): (Vec<&Task>, Vec<&Task>) =
                left.into_iter().partition(|t| t.depends_on.iter().all(|d| done.contains(d.as_str())));
            if ready.is_empty() {
                // A cycle validation missed: run the rest one at a time rather than never.
                waves.extend(rest.iter().map(|t| vec![t.id.clone()]));
                break;
            }
            done.extend(ready.iter().map(|t| t.id.as_str()));
            waves.push(ready.iter().map(|t| t.id.clone()).collect());
            left = rest;
        }
        waves
    }
}

/// The cost and time a plan is quoted at, shown with the plan approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildQuote {
    pub tasks: usize,
    pub waves: usize,
    /// Expected agent steps (model-backed playbook steps), not counting retries beyond one.
    pub steps: u32,
    pub model_tokens: u64,
    pub minutes: u32,
}

/// What the gate had failing before any change: `build_v2`'s `baseline.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// The gate's commands, in order.
    pub commands: Vec<String>,
    /// Ids that failed on the first run and again on the retry.
    pub failing: Vec<String>,
    /// Ids that failed once and passed on the retry (or the other way round).
    pub flaky: Vec<String>,
    /// The commit it was taken at.
    pub sha: String,
}

impl Baseline {
    /// Whether `id` failing says nothing about the change.
    pub fn excuses(&self, id: &str) -> bool {
        self.failing.iter().any(|f| f == id) || self.flaky.iter().any(|f| f == id)
    }
}

/// One run of the gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRun {
    /// Every command exited 0.
    pub ok: bool,
    /// Failing ids: test ids where the runner names them, else one per reported error, else
    /// `<kind> #n` per error line ([`super::gate::failing_ids`]).
    pub failing: BTreeSet<String>,
    /// The tail of each failing step's output.
    pub evidence: String,
}

/// The gate against the baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCheck {
    /// Failing now, not in the baseline, and failing again on the retry.
    pub new_failures: Vec<String>,
    /// Failed once now, passed on the retry.
    pub flaky: Vec<String>,
    /// Baseline failures that pass now.
    pub fixed: Vec<String>,
    pub evidence: String,
}

impl GateCheck {
    pub fn green(&self) -> bool {
        self.new_failures.is_empty()
    }
}

/// A reviewer's lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lens {
    Correctness,
    Security,
    Performance,
}

impl Lens {
    pub const ALL: [Lens; 3] = [Lens::Correctness, Lens::Security, Lens::Performance];

    pub fn name(self) -> &'static str {
        match self {
            Lens::Correctness => "correctness",
            Lens::Security => "security",
            Lens::Performance => "performance",
        }
    }

    /// The role card that reviews through this lens.
    pub fn role(self) -> &'static str {
        match self {
            Lens::Correctness => "review-correctness",
            Lens::Security => "review-security",
            Lens::Performance => "review-performance",
        }
    }
}

/// How much a finding matters. `Blocker` and `Major` must be fixed before delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Major => "major",
            Severity::Minor => "minor",
            Severity::Nit => "nit",
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "blocker" | "blocking" | "critical" => Some(Severity::Blocker),
            "major" | "high" => Some(Severity::Major),
            "minor" | "medium" | "low" => Some(Severity::Minor),
            "nit" | "info" | "style" => Some(Severity::Nit),
            _ => None,
        }
    }

    pub fn blocks(self) -> bool {
        matches!(self, Severity::Blocker | Severity::Major)
    }
}

/// One review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub lens: Lens,
    pub severity: Severity,
    pub file: String,
    #[serde(default)]
    pub line: u32,
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub fix: String,
}

impl Finding {
    /// Identity for stall detection: where and what, not how it is worded.
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.lens.name(), self.file, self.severity.name())
    }

    pub fn line(&self) -> String {
        let at = if self.line > 0 { format!("{}:{}", self.file, self.line) } else { self.file.clone() };
        format!("[{} {}] {at}: {}", self.lens.name(), self.severity.name(), self.claim)
    }
}

/// What became of one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub id: String,
    /// `done`, `failed`, `serial` (re-implemented on the merged branch), `dropped`.
    pub status: String,
    pub rounds: u32,
    #[serde(default)]
    pub replans: u32,
    #[serde(default)]
    pub hub_requests: u32,
    #[serde(default)]
    pub note: String,
}
