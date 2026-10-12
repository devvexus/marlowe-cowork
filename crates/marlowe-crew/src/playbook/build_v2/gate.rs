//! The gate: the project's own format, lint, typecheck, build and test commands (from
//! `marlowe-verify`'s profile), run through the run's guarded shell, read for failing test ids,
//! and compared against the baseline taken before any change. "Green" means no new failures:
//! what already failed does not block, and a new failure is retried once before it counts
//! (one that passes on the retry is flaky, reported, not blocking).

use std::collections::BTreeSet;

use marlowe_verify::profile::{Kind, ProjectProfile};

use super::types::{Baseline, GateCheck, GateRun};
use crate::playbook::{tail_chars, ShellResult, StepRunner};
use crate::sandbox::Sandbox;
use crate::CrewError;

/// Characters of a failing step's output kept as evidence.
const EVIDENCE_CHARS: usize = 4_000;

/// One gate command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateStep {
    pub kind: Kind,
    /// The command line, run from the sandbox root.
    pub command: String,
}

/// The gate's commands, in gate order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateSpec {
    pub steps: Vec<GateStep>,
}

impl GateSpec {
    /// Every gate command the profile has whose program is installed.
    pub fn from_profile(p: &ProjectProfile) -> GateSpec {
        let mut steps = Vec::new();
        for kind in Kind::GATE {
            for c in p.commands_of(kind).into_iter().filter(|c| c.available) {
                let line = c.cmd.display();
                let command = if c.cwd.trim().is_empty() || c.cwd == "." { line } else { format!("cd \"{}\" && {line}", c.cwd) };
                steps.push(GateStep { kind, command });
            }
        }
        GateSpec { steps }
    }

    /// The steps that say whether the tree builds: build and typecheck (the integrator's check).
    pub fn build_only(&self) -> GateSpec {
        GateSpec { steps: self.steps.iter().filter(|s| matches!(s.kind, Kind::Build | Kind::Typecheck)).cloned().collect() }
    }

    pub fn commands(&self) -> Vec<String> {
        self.steps.iter().map(|s| format!("{}: {}", s.kind.name(), s.command)).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The ids a step's result fails with: the test ids the runner names, else `<kind>` when it
/// failed without naming any.
pub fn failing_ids(kind: Kind, r: &ShellResult) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    if kind == Kind::Test {
        ids.extend(marlowe_verify::output::parse(&r.output, true).failing);
    }
    if !r.ok && ids.is_empty() {
        ids.insert(format!("<{}>", kind.name()));
    }
    ids
}

/// Runs every step in `sandbox`. A failing build stops the run (the tests cannot mean anything).
pub fn run(runner: &dyn StepRunner, sandbox: &Sandbox, spec: &GateSpec) -> Result<GateRun, CrewError> {
    let mut out = GateRun { ok: true, ..Default::default() };
    for step in &spec.steps {
        let r = runner.shell_in(sandbox, &step.command)?;
        let ids = failing_ids(step.kind, &r);
        if !r.ok || !ids.is_empty() {
            out.ok = false;
            out.evidence.push_str(&format!("$ {}\n{}\n", step.command, tail_chars(&r.output, EVIDENCE_CHARS)));
        }
        out.failing.extend(ids);
        if !r.ok && step.kind == Kind::Build {
            break;
        }
    }
    Ok(out)
}

/// The baseline: the gate on the untouched tree, each failure retried once. Failing both times
/// is `failing`; failing once is `flaky`.
pub fn baseline(runner: &dyn StepRunner, sandbox: &Sandbox, spec: &GateSpec, sha: &str) -> Result<Baseline, CrewError> {
    let first = run(runner, sandbox, spec)?;
    let mut b = Baseline { commands: spec.commands(), sha: sha.to_string(), ..Default::default() };
    if first.failing.is_empty() {
        return Ok(b);
    }
    let second = run(runner, sandbox, spec)?;
    b.failing = first.failing.intersection(&second.failing).cloned().collect();
    b.flaky = first.failing.symmetric_difference(&second.failing).cloned().collect();
    Ok(b)
}

/// The gate against `baseline`: new failures are retried once; those that pass then are flaky.
pub fn check(runner: &dyn StepRunner, sandbox: &Sandbox, spec: &GateSpec, baseline: &Baseline) -> Result<GateCheck, CrewError> {
    let first = run(runner, sandbox, spec)?;
    let fixed: Vec<String> = baseline.failing.iter().filter(|f| !first.failing.contains(*f)).cloned().collect();
    let new: BTreeSet<String> = first.failing.iter().filter(|f| !baseline.excuses(f)).cloned().collect();
    if new.is_empty() {
        return Ok(GateCheck { fixed, ..Default::default() });
    }
    let retry = run(runner, sandbox, spec)?;
    let new_failures: Vec<String> = new.iter().filter(|f| retry.failing.contains(*f)).cloned().collect();
    let flaky: Vec<String> = new.iter().filter(|f| !retry.failing.contains(*f)).cloned().collect();
    let evidence = if new_failures.is_empty() { String::new() } else { retry.evidence };
    Ok(GateCheck { new_failures, flaky, fixed, evidence })
}

/// A task's acceptance command in `sandbox`: whether it passed, its failing ids, its output.
pub fn accept(runner: &dyn StepRunner, sandbox: &Sandbox, command: &str) -> Result<(bool, BTreeSet<String>, String), CrewError> {
    let r = runner.shell_in(sandbox, command)?;
    let ids = failing_ids(Kind::Test, &r);
    Ok((r.ok, ids, tail_chars(&r.output, EVIDENCE_CHARS)))
}
