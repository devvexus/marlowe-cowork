//! build_v2 (design 3.6): a lead plans a task DAG, freezes the hub files after a contract stage,
//! engineers implement the tasks in parallel child worktrees, the runner integrates them
//! mechanically, the gate compares against a baseline, three lens reviewers read the diff, and
//! the fix loop runs until nothing blocks. The diff then waits for the user's merge approval.
//!
//! | Stage | Who | Checkpoint |
//! |---|---|---|
//! | 0 scope | runner | `b2:baseline` (`baseline.json`: failing ids, each failure retried once -> flaky) |
//! | 1 plan | lead | `b2:plan` step, `b2:plan:approved` (after the user's approval) |
//! | 2 contract | lead | `b2:contract` step, `b2:contract:done`; hubs frozen from here |
//! | 3 implement | engineers | `b2:task:<id>:r<n>` steps in child worktrees, `b2:task:<id>:done` |
//! | 4 integrate | runner, integrator | `b2:merge:<id>` (merge --no-ff in topo order, lockfiles regenerated, serial fallback) |
//! | 5 verify | runner | `b2:verify:<n>` (no new failures vs baseline, accept tests) |
//! | 6 review | 3 lenses | `b2:review:<n>:<lens>` steps |
//! | 7 fix | engineer, lead | `b2:fix:<n>`, `b2:fixplan:<n>` (stall -> replan -> ask) |
//! | 8 deliver | - | diff, gate evidence vs baseline, review table, artifacts |
//!
//! Every agent call checkpoints per model call and per tool batch (the runner's hooks); the
//! mechanical work between them is idempotent, so a resumed run picks up where it stopped.

mod gate;
mod git;
mod parse;
mod review;
mod types;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use marlowe_proto::{Confidence, Dispatch};
use marlowe_verify::hubs::HubClass;
use marlowe_verify::profile::ProjectProfile;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub use gate::{GateSpec, GateStep};
pub use parse::{hub_requests, json_value, parse_findings, parse_plan, MAX_TASKS};
pub use review::{checklist, review_diff, DIFF_CHARS, REVIEW_TOOLS};
pub use types::{
    Baseline, BuildPlan, BuildQuote, Finding, GateCheck, GateRun, Lens, Severity, Task, TaskOutcome,
};

use super::{tail_chars, StepRunner, StepSpec};
use crate::sandbox::Sandbox;
use crate::CrewError;

/// Read-only tools for planning.
const LOOK: [&str; 3] = ["read", "glob", "grep"];

/// Characters of failure evidence handed back to an agent.
const FAIL_CHARS: usize = 6_000;

/// Files of the repository map shown to the lead.
const MAP_FILES: usize = 400;

/// A lockfile and the command that regenerates it (run from the repository root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRule {
    pub file: String,
    pub regen: String,
}

/// build_v2 tunables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildV2Config {
    /// Engineers working at once (local models are throttled further by the run's slots).
    pub max_parallel: usize,
    /// Rounds an engineer gets per task.
    pub max_task_rounds: u32,
    /// Integrator rounds on a failing build after a merge, before the serial fallback.
    pub max_integrate_rounds: u32,
    /// Fix rounds after review before the run asks to extend them.
    pub max_fix_rounds: u32,
    /// Skip the plan approval (headless runs that approved up front).
    pub auto_approve_plan: bool,
    /// Wall-clock budget; past it the run asks before the next stage.
    pub max_wall: Option<Duration>,
    /// Lockfiles beyond the ones the project profile knows.
    pub lockfiles: Vec<LockRule>,
}

impl Default for BuildV2Config {
    fn default() -> Self {
        Self {
            max_parallel: 3,
            max_task_rounds: 4,
            max_integrate_rounds: 2,
            max_fix_rounds: 3,
            auto_approve_plan: false,
            max_wall: Some(Duration::from_secs(12 * 3600)),
            lockfiles: Vec::new(),
        }
    }
}

/// What build_v2 needs from its run beyond the [`StepRunner`]: approvals, data checkpoints,
/// child worktrees, the project profile, memory. The supervisor's run thread implements it.
pub trait BuildHost: Send + Sync {
    /// The run's one-line current action.
    fn progress(&self, action: &str);

    /// Cancel and pause, between mechanical stages.
    fn check(&self) -> Result<(), CrewError>;

    /// Asks the user to approve the plan; `Ok(false)` is a rejection.
    fn propose_plan(&self, plan: &BuildPlan, quote: &BuildQuote) -> Result<bool, CrewError>;

    /// The plan was approved: the run budget rises to cover the quote.
    fn plan_approved(&self, _plan: &BuildPlan, _quote: &BuildQuote) {}

    /// Asks the user a yes/no question on the run's behalf (a stalled fix loop, more rounds, more
    /// time).
    fn ask_user(&self, summary: &str, detail: &str) -> Result<bool, CrewError>;

    /// A data checkpoint saved earlier by this run.
    fn load(&self, key: &str) -> Option<String>;

    /// Saves a data checkpoint (a transcript row labelled `label`).
    fn save(&self, key: &str, label: &str, data: &str);

    /// A child worktree for `task`, cut from `parent`'s HEAD, with `frozen` hub files.
    fn spawn_worktree(&self, parent: &Sandbox, task: &str, frozen: &[String]) -> Result<Sandbox, CrewError>;

    /// The project's commands, hub files and lockfiles.
    fn profile(&self, root: &Path) -> ProjectProfile {
        marlowe_verify::profile::detect(&marlowe_os::workspace::LocalFs, root)
    }

    /// Where `baseline.json` and the other delivery files go.
    fn artifacts_dir(&self) -> Option<PathBuf> {
        None
    }

    /// Wall-clock time the run has used.
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }

    /// CI status of the run's branch, when it has been pushed (never by the crew).
    fn ci_status(&self) -> Option<String> {
        None
    }

    /// What memory holds about `query` (lines, already trust-labelled). No model call.
    fn recall(&self, _query: &str) -> Vec<String> {
        Vec::new()
    }

    /// Remembers the build's outcome (extractive text, agent provenance).
    fn remember(&self, _text: &str) {}

    /// A progress report for the client's DAG strip: `dag {json}` with the stage, the hub files
    /// and every task's state. Shown live, not stored.
    fn dag(&self, _report: &str) {}
}

fn load_json<T: DeserializeOwned>(host: &dyn BuildHost, key: &str) -> Option<T> {
    serde_json::from_str(&host.load(key)?).ok()
}

fn save_json<T: Serialize>(host: &dyn BuildHost, key: &str, label: &str, v: &T) {
    if let Ok(s) = serde_json::to_string(v) {
        host.save(key, label, &s);
    }
}

fn write_artifact<T: Serialize>(host: &dyn BuildHost, name: &str, v: &T) -> Option<String> {
    let dir = host.artifacts_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(v).ok()?).ok()?;
    Some(path.display().to_string())
}

fn is_stop(e: &CrewError) -> bool {
    matches!(e, CrewError::Cancelled | CrewError::Stopped)
}

/// A step's result with cancel and stop split off: `Err` only for those (they end the run);
/// any other failure is the `Ok(Err)` the stage handles (one reviewer or integrator that gave no
/// answer must not throw away the build).
fn soft<T>(r: Result<T, CrewError>) -> Result<Result<T, CrewError>, CrewError> {
    match r {
        Err(e) if is_stop(&e) => Err(e),
        r => Ok(r),
    }
}

fn step(key: String, role: &str, label: String, prompt: String, tools: Option<&[&str]>, sandbox: Option<Sandbox>) -> StepSpec {
    StepSpec {
        key,
        role: role.into(),
        label,
        prompt,
        tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        effort: None,
        max_output: None,
        sandbox,
    }
}

/// Everything the stages share.
struct Build<'a> {
    runner: &'a dyn StepRunner,
    host: &'a dyn BuildHost,
    cfg: &'a BuildV2Config,
    goal: &'a str,
    run: Sandbox,
    spec: GateSpec,
    baseline: Baseline,
    locks: Vec<LockRule>,
    /// Pre-existing failures, for prompts.
    known_failing: String,
    events: Mutex<Vec<String>>,
    dag: Mutex<Dag>,
    /// The baseline while tasks are implemented: the original one plus what failed right after
    /// the contract (the other tasks' acceptance tests fail until those tasks land). Verify
    /// compares against the original again.
    work: Mutex<Baseline>,
    /// Held across a hub change: the lead's step, its commit, and the asking child's merge.
    hubs: Mutex<()>,
}

/// What the DAG strip shows.
#[derive(Default)]
struct Dag {
    stage: String,
    hubs: Vec<String>,
    tasks: Vec<Task>,
    states: BTreeMap<String, String>,
}

impl Build<'_> {
    /// Moves the DAG strip to `stage` and reports it.
    fn stage(&self, stage: &str, action: &str) {
        self.dag.lock().unwrap_or_else(|e| e.into_inner()).stage = stage.into();
        self.host.progress(action);
        self.report();
    }

    /// The plan is known: its tasks and hubs join the report.
    fn planned(&self, plan: &BuildPlan) {
        let mut d = self.dag.lock().unwrap_or_else(|e| e.into_inner());
        d.tasks = plan.tasks.clone();
        d.hubs = plan.hubs.clone();
    }

    /// A task's state: `waiting`, `running`, `blocked`, `done`, `failed`, `serial`.
    fn task_state(&self, id: &str, state: &str) {
        self.dag.lock().unwrap_or_else(|e| e.into_inner()).states.insert(id.into(), state.into());
        self.report();
    }

    fn report(&self) {
        let d = self.dag.lock().unwrap_or_else(|e| e.into_inner());
        let tasks: Vec<serde_json::Value> = d
            .tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "goal": t.goal,
                    "deps": t.depends_on,
                    "state": d.states.get(&t.id).map(String::as_str).unwrap_or("waiting"),
                })
            })
            .collect();
        let v = serde_json::json!({"stage": d.stage, "hubs": d.hubs, "tasks": tasks});
        self.host.dag(&format!("dag {v}"));
    }

    fn event(&self, line: String) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    }

    fn is_lockfile(&self, file: &str) -> bool {
        self.locks.iter().any(|l| l.file.eq_ignore_ascii_case(file))
    }

    /// Past the wall-clock budget, asks once per overrun whether to go on. `Ok(false)` stops
    /// the build where it is and delivers what there is.
    fn within_wall(&self, at: &str) -> Result<bool, CrewError> {
        let Some(max) = self.cfg.max_wall else { return Ok(true) };
        let extended: u64 = self.host.load("b2:wall").and_then(|s| s.parse().ok()).unwrap_or(0);
        let limit = max + Duration::from_secs(extended);
        let used = self.host.elapsed();
        if used <= limit {
            return Ok(true);
        }
        let hours = used.as_secs_f64() / 3600.0;
        let summary = format!("build run has used {hours:.1} h of its {:.1} h budget at {at}; continue for another half?", limit.as_secs_f64() / 3600.0);
        if self.host.ask_user(&summary, "Declining delivers what is built so far.")? {
            let more = extended + max.as_secs() / 2;
            self.host.save("b2:wall", "wall-clock budget extended", &more.to_string());
            Ok(true)
        } else {
            self.event(format!("stopped at {at}: past the wall-clock budget"));
            Ok(false)
        }
    }
}

/// Runs build_v2 for `goal` on the run's worktree.
pub fn build_v2(runner: &dyn StepRunner, host: &dyn BuildHost, goal: &str, cfg: &BuildV2Config) -> Result<Dispatch, CrewError> {
    let run = runner.sandbox().clone();
    let root = run.root.clone();

    // ---------------------------------------------------------------- 0 scope
    host.progress("build 0/8 scope: profile and baseline");
    let profile = host.profile(&root);
    let spec = GateSpec::from_profile(&profile);
    let locks = lock_rules(&profile, cfg);
    let baseline: Baseline = match load_json(host, "b2:baseline") {
        Some(b) => b,
        None => {
            let sha = git::head(&root)?;
            let b = if spec.is_empty() {
                Baseline { sha, ..Default::default() }
            } else {
                gate::baseline(runner, &run, &spec, &sha)?
            };
            let label = format!("baseline: {} failing, {} flaky", b.failing.len(), b.flaky.len());
            save_json(host, "b2:baseline", &label, &b);
            b
        }
    };
    let baseline_file = write_artifact(host, "baseline.json", &baseline);
    host.check()?;
    let known_failing = if baseline.failing.is_empty() && baseline.flaky.is_empty() {
        "none".to_string()
    } else {
        let mut s = baseline.failing.join(", ");
        if !baseline.flaky.is_empty() {
            s.push_str(&format!(" (flaky: {})", baseline.flaky.join(", ")));
        }
        s
    };
    let b = Build {
        runner,
        host,
        cfg,
        goal,
        run,
        spec,
        baseline,
        locks,
        known_failing,
        events: Mutex::new(Vec::new()),
        dag: Mutex::new(Dag { stage: "scope".into(), ..Default::default() }),
        work: Mutex::new(Baseline::default()),
        hubs: Mutex::new(()),
    };
    *b.work.lock().unwrap_or_else(|e| e.into_inner()) = b.baseline.clone();
    b.report();

    // ---------------------------------------------------------------- 1 plan
    let known_hubs: Vec<String> =
        profile.hub_files.iter().filter(|h| h.class != HubClass::Lockfile).map(|h| h.path.clone()).collect();
    let Some(plan) = plan_stage(&b, &profile, &known_hubs)? else {
        return Ok(Dispatch {
            summary: "The build plan was rejected; nothing was changed.".into(),
            artifacts: Vec::new(),
            confidence: Confidence::Low,
            citations: Vec::new(),
            diff: None,
        });
    };
    let plan_file = write_artifact(host, "plan.json", &plan);
    b.planned(&plan);
    let frozen: Vec<String> = plan.hubs.iter().filter(|h| !b.is_lockfile(h)).cloned().collect();

    // ---------------------------------------------------------------- 2 contract
    contract_stage(&b, &plan, &frozen)?;

    // ---------------------------------------------------------------- 3 + 4, wave by wave
    let mut outcomes: BTreeMap<String, TaskOutcome> = BTreeMap::new();
    let mut children: BTreeMap<String, Sandbox> = BTreeMap::new();
    let waves = plan.waves();
    let mut stopped = false;
    for (w, wave) in waves.iter().enumerate() {
        host.check()?;
        if !b.within_wall(&format!("wave {}", w + 1))? {
            stopped = true;
            break;
        }
        // A task whose dependency failed cannot build on it.
        let (ready, blocked): (Vec<&String>, Vec<&String>) = wave.iter().partition(|id| {
            plan.task(id).is_some_and(|t| t.depends_on.iter().all(|d| outcomes.get(d).is_some_and(|o| o.status != "failed" && o.status != "dropped")))
        });
        for id in &blocked {
            outcomes.insert(id.to_string(), TaskOutcome {
                id: id.to_string(),
                status: "dropped".into(),
                rounds: 0,
                replans: 0,
                hub_requests: 0,
                note: "a task it depends on failed".into(),
            });
        }
        for id in &blocked {
            b.task_state(id, "failed");
        }
        b.stage("implement", &format!("build 3/8 implement: wave {}/{} ({})", w + 1, waves.len(), ready.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        let tasks: Vec<&Task> = ready.iter().filter_map(|id| plan.task(id)).collect();
        for (id, r) in implement_wave(&b, &plan, &tasks, &frozen)? {
            match r {
                Ok((child, o)) => {
                    children.insert(id.clone(), child);
                    outcomes.insert(id, o);
                }
                Err(e) => {
                    outcomes.insert(id.clone(), TaskOutcome {
                        id,
                        status: "failed".into(),
                        rounds: 0,
                        replans: 0,
                        hub_requests: 0,
                        note: e.to_string(),
                    });
                }
            }
        }
        b.stage("integrate", &format!("build 4/8 integrate: wave {}/{}", w + 1, waves.len()));
        for id in wave {
            let (Some(o), Some(child)) = (outcomes.get(id).cloned(), children.get(id).cloned()) else { continue };
            if o.status != "done" {
                continue;
            }
            let Some(task) = plan.task(id) else { continue };
            let merged = integrate_task(&b, task, &child, o)?;
            b.task_state(id, if merged.status == "serial" { "serial" } else if merged.status == "failed" { "failed" } else { "done" });
            outcomes.insert(id.clone(), merged);
        }
    }

    // ---------------------------------------------------------------- 5-7 verify, review, fix
    let (check, accept_fails, findings) = if stopped {
        (None, Vec::new(), BTreeMap::new())
    } else {
        let (c, a, f) = verify_review_fix(&b, &plan, &outcomes)?;
        (Some(c), a, f)
    };

    // ---------------------------------------------------------------- 8 deliver
    b.stage("deliver", "build 8/8 deliver");
    for (id, child) in &children {
        // A serial task's branch is superseded by its re-implementation.
        let merged = git::is_merged(&b.run.root, child.branch.as_deref().unwrap_or(""))
            || outcomes.get(id).is_some_and(|o| o.status == "serial");
        let _ = child.remove();
        if merged {
            if let (Some(origin), Some(branch)) = (child.origin.as_deref(), child.branch.as_deref()) {
                let _ = crate::sandbox::git(origin, &["branch", "-D", branch]);
            }
        } else if let Some(o) = outcomes.get_mut(id) {
            if !o.note.is_empty() {
                o.note.push_str("; ");
            }
            o.note.push_str(&format!("work kept on `{}`", child.branch.as_deref().unwrap_or("?")));
        }
    }
    deliver(&b, &plan, &outcomes, check.as_ref(), &accept_fails, &findings, stopped, [baseline_file, plan_file])
}

/// The profile's lockfiles plus the configured ones.
fn lock_rules(p: &ProjectProfile, cfg: &BuildV2Config) -> Vec<LockRule> {
    let mut out: Vec<LockRule> = p
        .lockfiles
        .iter()
        .map(|l| {
            let dir = Path::new(&l.file).parent().map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let regen = l.regen.display();
            LockRule { file: l.file.replace('\\', "/"), regen: if dir.is_empty() { regen } else { format!("cd \"{dir}\" && {regen}") } }
        })
        .collect();
    for l in &cfg.lockfiles {
        if !out.iter().any(|o| o.file.eq_ignore_ascii_case(&l.file)) {
            out.push(l.clone());
        }
    }
    out
}

/// `git ls-files`, cut to [`MAP_FILES`].
fn repo_map(root: &Path) -> String {
    let files = crate::sandbox::git(root, &["ls-files"]).unwrap_or_default();
    let all: Vec<&str> = files.lines().collect();
    let mut s = all.iter().take(MAP_FILES).copied().collect::<Vec<_>>().join("\n");
    if all.len() > MAP_FILES {
        s.push_str(&format!("\n[… {} more files]", all.len() - MAP_FILES));
    }
    s
}

// -------------------------------------------------------------------- 1 plan

const PLAN_FORMAT: &str = "Reply with one ```json block: {\"tasks\": [{\"id\": \"t1\", \"goal\": \"what this task delivers\", \
\"files_owned\": [\"src/a.rs\"], \"interfaces\": \"signatures or types other tasks rely on\", \"depends_on\": [], \
\"accept_cmd\": \"the command that runs this task's acceptance test\"}], \"hubs\": [\"files every task may need: manifests, \
module declarations, registries\"]}. Ids are short lower-case words. A task owns the files it changes; two tasks that \
must change the same non-hub file depend on each other. Prefer few, independent tasks; one task is right for a small goal.";

fn plan_stage(b: &Build<'_>, profile: &ProjectProfile, known_hubs: &[String]) -> Result<Option<BuildPlan>, CrewError> {
    if let Some(p) = load_json::<BuildPlan>(b.host, "b2:plan:approved") {
        return Ok(Some(p));
    }
    if b.host.load("b2:plan:rejected").is_some() {
        return Ok(None);
    }
    b.stage("plan", "build 1/8 plan");
    let memory = b.host.recall(b.goal);
    let memory = if memory.is_empty() { String::new() } else { format!("\nWhat memory holds about this:\n{}\n", memory.join("\n")) };
    let gate = if b.spec.is_empty() { "none found (say how each task is checked)".to_string() } else { b.spec.commands().join("; ") };
    let hubs = if known_hubs.is_empty() { "none detected".into() } else { known_hubs.join(", ") };
    let warnings = if profile.warnings.is_empty() { String::new() } else { format!("\nProfile notes: {}\n", profile.warnings.join("; ")) };
    let prompt = format!(
        "## Stage: plan\nGoal: {goal}\n\nYou are the lead of a build. Read the code the goal touches, then split the work into \
         a task DAG that engineers implement in parallel, each in its own worktree.\n\nRepository files:\n{map}\n\n\
         Gate commands: {gate}\nAlready failing before any change (not ours to fix): {failing}\n\
         Hub files the project profile found: {hubs}\n{warnings}{memory}\n{PLAN_FORMAT}",
        goal = b.goal,
        map = repo_map(&b.run.root),
        failing = b.known_failing,
    );
    let r = b.runner.step(step("b2:plan".into(), "lead", "plan".into(), prompt.clone(), Some(&LOOK[..]), None))?;
    let plan = match parse_plan(&r.text, known_hubs) {
        Ok(p) => p,
        Err(errors) => {
            let again = b.runner.step(step(
                "b2:plan:repair".into(),
                "lead",
                "plan (repair)".into(),
                format!(
                    "{prompt}\n\nYour plan could not be used:\n- {}\n\nYour reply was:\n{}\n\nReply with the corrected plan only.",
                    errors.join("\n- "),
                    r.text.chars().take(6_000).collect::<String>()
                ),
                Some(&LOOK[..]),
                None,
            ))?;
            parse_plan(&again.text, known_hubs).map_err(|e| CrewError::Step { step: "plan".into(), message: format!("the plan does not validate: {}", e.join("; ")) })?
        }
    };
    let quote = quote(&plan);
    // Only the user's approval raises the budget to the quote: an auto-approved plan keeps the
    // run's limits, so an overrun still asks (non-negotiable 5).
    if !b.cfg.auto_approve_plan {
        if !b.host.propose_plan(&plan, &quote)? {
            b.host.save("b2:plan:rejected", "plan rejected", "rejected");
            return Ok(None);
        }
        b.host.plan_approved(&plan, &quote);
    }
    save_json(b.host, "b2:plan:approved", &format!("plan approved: {} tasks in {} waves", plan.tasks.len(), plan.waves().len()), &plan);
    Ok(Some(plan))
}

/// The plan's cost and time estimate: plan + contract + two rounds per task + an integration
/// fix per wave + three reviews + two fix rounds; 40k tokens and 3 minutes per agent step.
pub fn quote(plan: &BuildPlan) -> BuildQuote {
    let waves = plan.waves().len();
    let steps = 2 + 2 * plan.tasks.len() as u32 + waves as u32 + 3 + 2;
    BuildQuote { tasks: plan.tasks.len(), waves, steps, model_tokens: u64::from(steps) * 40_000, minutes: steps * 3 }
}

/// The plan as the approval shows it.
pub fn render_plan(plan: &BuildPlan, quote: &BuildQuote) -> String {
    let mut s = String::new();
    for t in &plan.tasks {
        let deps = if t.depends_on.is_empty() { String::new() } else { format!(" (after {})", t.depends_on.join(", ")) };
        s.push_str(&format!("- {}{deps}: {}\n", t.id, t.goal));
        if !t.files_owned.is_empty() {
            s.push_str(&format!("  owns: {}\n", t.files_owned.join(", ")));
        }
        if let Some(a) = &t.accept_cmd {
            s.push_str(&format!("  accept: `{a}`\n"));
        }
    }
    if !plan.hubs.is_empty() {
        s.push_str(&format!("Hub files (frozen after the contract stage): {}\n", plan.hubs.join(", ")));
    }
    for n in &plan.notes {
        s.push_str(&format!("Note: {n}\n"));
    }
    s.push_str(&format!(
        "Estimate: {} waves, about {} agent steps, {} model tokens, {} minutes.",
        quote.waves, quote.steps, quote.model_tokens, quote.minutes
    ));
    s
}

// -------------------------------------------------------------------- 2 contract

/// What the contract stage left: its commit and the ids that failed right after it.
#[derive(serde::Serialize, serde::Deserialize)]
struct ContractDone {
    sha: String,
    expected: Vec<String>,
}

impl Build<'_> {
    fn work_baseline(&self) -> Baseline {
        self.work.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_expected(&self, expected: &[String]) {
        let mut w = self.work.lock().unwrap_or_else(|e| e.into_inner());
        for id in expected {
            if !w.excuses(id) {
                w.failing.push(id.clone());
            }
        }
    }
}

fn contract_stage(b: &Build<'_>, plan: &BuildPlan, frozen: &[String]) -> Result<(), CrewError> {
    if let Some(done) = load_json::<ContractDone>(b.host, "b2:contract:done") {
        b.set_expected(&done.expected);
        return Ok(());
    }
    b.stage("contract", "build 2/8 contract");
    let plan_json = serde_json::to_string_pretty(plan).unwrap_or_default();
    let prompt = format!(
        "## Stage: contract\nGoal: {goal}\n\nThe approved plan:\n```json\n{plan_json}\n```\n\nPrepare the shared ground every task \
         builds on, in your worktree, before the engineers start: every hub edit the tasks need (module declarations, \
         dependency entries, registry lines, migration slots, route stubs), the shared types and interfaces, and each \
         task's acceptance test, written so it fails now for the right reason (the feature is missing, not a syntax \
         error). Stubs that build are fine; do not implement the tasks. After you finish the hub files are frozen: {hubs}. \
         The tree must still build. Finish with what you prepared.",
        goal = b.goal,
        hubs = if frozen.is_empty() { "none".to_string() } else { frozen.join(", ") },
    );
    b.runner.step(step("b2:contract".into(), "lead", "contract".into(), prompt, None, None))?;
    git::commit(&b.run.root, "build_v2: contract")?;
    // The tree must build after the contract; the lead gets two rounds to make it.
    let build = b.spec.build_only();
    if !build.is_empty() {
        let mut round = 0;
        loop {
            let c = gate::check(b.runner, &b.run, &build, &b.baseline)?;
            if c.green() {
                break;
            }
            round += 1;
            if round > 2 {
                return Err(CrewError::Step {
                    step: "contract".into(),
                    message: format!("the tree does not build after the contract stage: {}", c.new_failures.join(", ")),
                });
            }
            b.runner.step(step(
                format!("b2:contract:fix{round}"),
                "lead",
                format!("contract fix {round}"),
                format!(
                    "## Stage: contract (fix {round})\nThe tree does not build after your contract changes:\n```\n{}\n```\nFix it; \
                     do not implement the tasks.",
                    tail_chars(&c.evidence, FAIL_CHARS)
                ),
                None,
                None,
            ))?;
            git::commit(&b.run.root, &format!("build_v2: contract fix {round}"))?;
        }
    }
    let mut notes = Vec::new();
    for t in &plan.tasks {
        if let Some(cmd) = &t.accept_cmd {
            let (ok, _, _) = gate::accept(b.runner, &b.run, cmd)?;
            if ok {
                notes.push(format!("{}: its acceptance test already passes, so it may not test the feature", t.id));
            }
        }
    }
    for n in &notes {
        b.event(n.clone());
    }
    // What fails now is expected until the tasks land (their acceptance tests, mostly).
    let expected: Vec<String> = if b.spec.is_empty() {
        Vec::new()
    } else {
        gate::run(b.runner, &b.run, &b.spec)?.failing.into_iter().filter(|f| !b.baseline.excuses(f)).collect()
    };
    b.set_expected(&expected);
    let done = ContractDone { sha: git::head(&b.run.root)?, expected };
    save_json(b.host, "b2:contract:done", &format!("contract committed; hubs frozen ({})", frozen.len()), &done);
    Ok(())
}

// -------------------------------------------------------------------- 3 implement

type TaskResult = Result<(Sandbox, TaskOutcome), CrewError>;

/// Runs a wave's tasks, at most `max_parallel` at once, each in its child worktree (made one at a
/// time first: git worktree bookkeeping is not for concurrent writers). A cancel or pause stops
/// the wave.
fn implement_wave(b: &Build<'_>, plan: &BuildPlan, tasks: &[&Task], frozen: &[String]) -> Result<Vec<(String, TaskResult)>, CrewError> {
    let mut kids = Vec::new();
    for t in tasks {
        kids.push(b.host.spawn_worktree(&b.run, &t.id, frozen)?);
    }
    let n = tasks.len();
    let limit = b.cfg.max_parallel.clamp(1, n.max(1));
    let slots: Vec<Mutex<Option<TaskResult>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..limit {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n {
                    break;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| implement_task(b, plan, tasks[i], &kids[i], frozen)))
                    .unwrap_or_else(|_| Err(CrewError::Step { step: tasks[i].id.clone(), message: "the task panicked".into() }));
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
        }
    });
    let mut out = Vec::new();
    for (t, slot) in tasks.iter().zip(slots) {
        let r = slot.into_inner().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| {
            Err(CrewError::Step { step: t.id.clone(), message: "the task never ran".into() })
        });
        if let Err(e) = &r {
            if is_stop(e) {
                return Err(match e {
                    CrewError::Cancelled => CrewError::Cancelled,
                    _ => CrewError::Stopped,
                });
            }
        }
        out.push((t.id.clone(), r));
    }
    Ok(out)
}

fn task_prompt(b: &Build<'_>, task: &Task, round: u32, frozen: &[String], revised: Option<&str>, feedback: &str) -> String {
    let mut p = format!(
        "## Task {id} (round {round} of {max})\nOverall goal: {goal}\nYour task: {tgoal}\n",
        id = task.id,
        max = b.cfg.max_task_rounds,
        goal = b.goal,
        tgoal = task.goal,
    );
    if !task.files_owned.is_empty() {
        p.push_str(&format!("Files you own: {}\n", task.files_owned.join(", ")));
    }
    if !task.interfaces.is_empty() {
        p.push_str(&format!("Interfaces: {}\n", task.interfaces));
    }
    if let Some(a) = &task.accept_cmd {
        p.push_str(&format!("Acceptance test: `{a}` must pass. The lead wrote it; it fails now.\n"));
    }
    if !frozen.is_empty() {
        p.push_str(&format!(
            "Hub files are frozen and you cannot change them: {}. If you need one changed, end your reply with a line \
             `NEED_HUB_CHANGE: <file>: <what and why>` and the lead will make the change.\n",
            frozen.join(", ")
        ));
    }
    p.push_str(&format!("Already failing before this build (not yours to fix): {}\n", b.known_failing));
    if let Some(r) = revised {
        p.push_str(&format!("\nThe lead's revised instructions for this task:\n{r}\n"));
    }
    if !feedback.is_empty() {
        p.push_str(&format!("\nWhat happened last round:\n{feedback}\n"));
    }
    p.push_str("\nWork in your worktree. Finish with what you changed and how you checked it.");
    p
}

fn implement_task(b: &Build<'_>, plan: &BuildPlan, task: &Task, child: &Sandbox, frozen: &[String]) -> TaskResult {
    let id = &task.id;
    let child = child.clone();
    if let Some(o) = load_json::<TaskOutcome>(b.host, &format!("b2:task:{id}:done")) {
        b.task_state(id, &o.status);
        return Ok((child, o));
    }
    let base = child.base.clone().unwrap_or_default();
    let run_branch = b.run.branch.clone().unwrap_or_default();
    b.task_state(id, "running");
    let mut o = TaskOutcome { id: id.clone(), status: "failed".into(), rounds: 0, replans: 0, hub_requests: 0, note: String::new() };
    let mut feedback = String::new();
    let mut revised: Option<String> = None;
    let mut last: Option<BTreeSet<String>> = None;
    for round in 1..=b.cfg.max_task_rounds {
        b.host.check()?;
        o.rounds = round;
        let r = b.runner.step(step(
            format!("b2:task:{id}:r{round}"),
            "engineer",
            format!("task {id} round {round}"),
            task_prompt(b, task, round, frozen, revised.as_deref(), &feedback),
            None,
            Some(child.clone()),
        ));
        let r = match r {
            Ok(r) => r,
            Err(e) if is_stop(&e) => return Err(e),
            Err(e) => {
                feedback = format!("Your last round ended without an answer ({e}).");
                continue;
            }
        };
        git::commit(&child.root, &format!("task {id}: round {round}"))?;
        feedback.clear();
        // Hub files changed anyway (a shell can write them): put back, and say how to ask. The
        // hubs as they stand are the run branch's at the last point this worktree took it in
        // (its base, or the lead's hub change it merged), never the base alone: the lead's
        // change must not read as the engineer's.
        let hubs_at = git::merge_base(&child.root, &run_branch).unwrap_or_else(|_| base.clone());
        let touched: Vec<String> = git::changed_since(&child.root, &hubs_at)?
            .into_iter()
            .filter(|f| frozen.iter().any(|h| h.eq_ignore_ascii_case(f)))
            .collect();
        if !touched.is_empty() {
            git::restore(&child.root, &hubs_at, &touched, &format!("task {id}: put frozen hub files back"))?;
            feedback.push_str(&format!(
                "You changed frozen hub files ({}); those changes were undone. Ask the lead with NEED_HUB_CHANGE instead.\n",
                touched.join(", ")
            ));
        }
        let requests = hub_requests(&r.text);
        if !requests.is_empty() {
            o.hub_requests += 1;
            b.task_state(id, "blocked");
            // One hub change at a time, through the child's merge of it: two leads editing the
            // run's worktree at once would commit each other's half-done edits.
            let _hubs = b.hubs.lock().unwrap_or_else(|e| e.into_inner());
            let made = amend_hubs(b, plan, task, round, &requests);
            b.task_state(id, "running");
            match made {
                Ok(()) => match git::merge(&child.root, &run_branch, &format!("task {id}: take the lead's hub change"))? {
                    git::Merged::Clean => feedback.push_str(&format!(
                        "The lead changed the hub files you asked for ({}); they are in your worktree now.\n",
                        requests.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>().join(", ")
                    )),
                    git::Merged::Conflicts(files) => {
                        git::abort(&child.root);
                        feedback.push_str(&format!(
                            "The lead changed the hub files you asked for, but that change conflicts with your work in {} \
                             and was not taken in; change your work so it no longer conflicts, then ask again.\n",
                            files.join(", ")
                        ));
                    }
                },
                Err(e) if is_stop(&e) => return Err(e),
                Err(e) => {
                    b.event(format!("task {id}: the lead could not make the hub change ({e})"));
                    feedback.push_str(&format!(
                        "The lead could not make the hub change you asked for ({e}); work with the hub files as they are.\n"
                    ));
                }
            }
            continue;
        }
        let (accept_ok, accept_ids, accept_out) = match &task.accept_cmd {
            Some(cmd) => gate::accept(b.runner, &child, cmd)?,
            None => (true, BTreeSet::new(), String::new()),
        };
        let check = gate::check(b.runner, &child, &b.spec, &b.work_baseline())?;
        if accept_ok && check.green() && touched.is_empty() {
            o.status = "done".into();
            if !check.flaky.is_empty() {
                o.note = format!("flaky on the way: {}", check.flaky.join(", "));
            }
            save_json(b.host, &format!("b2:task:{id}:done"), &format!("task {id} done in {round} round(s)"), &o);
            b.task_state(id, "done");
            return Ok((child, o));
        }
        if !accept_ok {
            feedback.push_str(&format!(
                "`{}` failed:\n```\n{}\n```\n",
                task.accept_cmd.as_deref().unwrap_or(""),
                tail_chars(&accept_out, FAIL_CHARS)
            ));
        }
        if !check.green() {
            feedback.push_str(&format!(
                "New failures against the baseline: {}\n```\n{}\n```\n",
                check.new_failures.join(", "),
                tail_chars(&check.evidence, FAIL_CHARS)
            ));
        }
        let mut signature: BTreeSet<String> = accept_ids;
        signature.extend(check.new_failures.iter().cloned());
        signature.extend(touched.iter().map(|f| format!("hub:{f}")));
        // The same failures two rounds running: the approach is stuck, the lead replans it.
        if last.as_ref() == Some(&signature) && round < b.cfg.max_task_rounds {
            o.replans += 1;
            let rp = soft(b.runner.step(step(
                format!("b2:task:{id}:replan{}", o.replans),
                "lead",
                format!("replan task {id}"),
                format!(
                    "## Replan task {id}\nOverall goal: {goal}\nTask: {tgoal}\n\nThe engineer has failed the same way two rounds \
                     running:\n{feedback}\nRead the task's files in the engineer's worktree ({root}) and the failure, then \
                     write revised instructions for the next round: what to change, where, and why the last approach \
                     failed. If the task cannot be done as planned, say what to do instead.",
                    goal = b.goal,
                    tgoal = task.goal,
                    root = child.root.display(),
                ),
                Some(&LOOK[..]),
                None,
            )))?;
            match rp {
                Ok(rp) => {
                    revised = Some(rp.text);
                    b.event(format!("task {id}: stalled on the same failures; the lead replanned it"));
                }
                Err(e) => b.event(format!("task {id}: stalled, and the lead's replan failed ({e})")),
            }
        }
        last = Some(signature);
    }
    o.note = "still failing after its rounds".into();
    b.task_state(id, "failed");
    save_json(b.host, &format!("b2:task:{id}:done"), &format!("task {id} failed"), &o);
    Ok((child, o))
}

/// The lead makes the hub changes an engineer asked for, on the run's branch.
fn amend_hubs(b: &Build<'_>, plan: &BuildPlan, task: &Task, round: u32, requests: &[(String, String)]) -> Result<(), CrewError> {
    let asks: Vec<String> = requests.iter().map(|(f, w)| format!("- {f}: {w}")).collect();
    let pre = git::head(&b.run.root)?;
    let made = b.runner.step(step(
        format!("b2:hub:{}:r{round}", task.id),
        "lead",
        format!("hub change for {}", task.id),
        format!(
            "## Hub change\nGoal: {goal}\nThe engineer on task `{id}` ({tgoal}) needs these hub files changed:\n{asks}\n\nHub \
             files: {hubs}. Make the smallest change that serves every task, keep the tree building, and say what you \
             changed.",
            goal = b.goal,
            id = task.id,
            tgoal = task.goal,
            asks = asks.join("\n"),
            hubs = plan.hubs.join(", "),
        ),
        None,
        None,
    ));
    if let Err(e) = made {
        // Half an edit must not ride along with the next commit on the run branch.
        if !is_stop(&e) {
            git::reset_hard(&b.run.root, &pre)?;
        }
        return Err(e);
    }
    git::commit(&b.run.root, &format!("build_v2: hub change for {}", task.id))?;
    b.event(format!("task {}: the lead changed hub files on request", task.id));
    Ok(())
}

// -------------------------------------------------------------------- 4 integrate

/// Merges a done task's branch into the run's branch: lockfiles take one side and are
/// regenerated, textual conflicts go to the integrator, a failing build gets up to
/// `max_integrate_rounds` integrator rounds, then the serial fallback re-implements the task on
/// the merged branch.
fn integrate_task(b: &Build<'_>, task: &Task, child: &Sandbox, mut o: TaskOutcome) -> Result<TaskOutcome, CrewError> {
    let id = &task.id;
    let key = format!("b2:merge:{id}");
    if let Some(done) = load_json::<TaskOutcome>(b.host, &key) {
        return Ok(done);
    }
    b.host.check()?;
    let root = &b.run.root;
    let branch = child.branch.clone().unwrap_or_default();
    let pre = git::head(root)?;
    let child_changed = git::changed_since(&child.root, child.base.as_deref().unwrap_or("HEAD")).unwrap_or_default();
    let mut regen: BTreeSet<String> = child_changed.iter().filter(|f| b.is_lockfile(f)).cloned().collect();
    let mut merged_ok = true;
    if !git::is_merged(root, &branch) {
        match git::merge(root, &branch, &format!("build_v2: merge task {id}"))? {
            git::Merged::Clean => {}
            git::Merged::Conflicts(files) => {
                let (locks, text): (Vec<String>, Vec<String>) = files.into_iter().partition(|f| b.is_lockfile(f));
                for f in &locks {
                    git::take_ours(root, f)?;
                    regen.insert(f.clone());
                }
                if !text.is_empty() {
                    let resolved = soft(b.runner.step(step(
                        format!("b2:integrate:{id}:conflicts"),
                        "integrator",
                        format!("integrate {id}: conflicts"),
                        format!(
                            "## Integrate: resolve conflicts\nGoal: {goal}\nMerging task `{id}` ({tgoal}) into the build branch \
                             stopped on conflicts in: {files}.\nResolve each file in your worktree so both sides' intent \
                             survives; remove every conflict marker. Do not commit; say what you kept.",
                            goal = b.goal,
                            tgoal = task.goal,
                            files = text.join(", "),
                        ),
                        None,
                        None,
                    )))?;
                    let left = match resolved {
                        Ok(_) => git::with_markers(root, &text),
                        Err(e) => {
                            b.event(format!("task {id}: the integrator gave no answer on the conflicts ({e})"));
                            text.clone()
                        }
                    };
                    if !left.is_empty() {
                        git::abort(root);
                        b.event(format!("task {id}: conflicts in {} were not resolved; serial fallback", left.join(", ")));
                        merged_ok = false;
                    }
                }
                if merged_ok {
                    git::conclude(root)?;
                }
            }
        }
    }
    if merged_ok {
        regenerate(b, &regen)?;
        if build_green(b, task, &mut o)? {
            o.status = "done".into();
            save_json(b.host, &key, &format!("task {id} merged"), &o);
            return Ok(o);
        }
    }
    // Serial fallback: the task again, by one engineer, on the branch as it stands without it.
    git::reset_hard(root, &pre)?;
    b.event(format!("task {id}: merged build failed; re-implemented serially on the merged branch"));
    b.task_state(id, "serial");
    b.host.progress(&format!("build 4/8 integrate: serial fallback for {id}"));
    let serial = soft(b.runner.step(step(
        format!("b2:serial:{id}"),
        "engineer",
        format!("serial {id}"),
        format!(
            "## Serial re-implementation of task {id}\nOverall goal: {goal}\nTask: {tgoal}\nInterfaces: {ifaces}\n\nThis task was \
             built in parallel, but merged with the other tasks the build failed. Implement it again here, on the merged \
             branch, so it fits what the other tasks did.{accept}\nAlready failing before this build: {failing}\nFinish with \
             what you changed and how you checked it.",
            goal = b.goal,
            tgoal = task.goal,
            ifaces = if task.interfaces.is_empty() { "-" } else { &task.interfaces },
            accept = task.accept_cmd.as_deref().map(|a| format!(" `{a}` must pass.")).unwrap_or_default(),
            failing = b.known_failing,
        ),
        None,
        None,
    )))?;
    if let Err(e) = serial {
        git::reset_hard(root, &pre)?;
        o.status = "failed".into();
        o.note = format!("the serial re-implementation gave no answer ({e}); left out");
        save_json(b.host, &key, &format!("task {id}: failed"), &o);
        return Ok(o);
    }
    git::commit(root, &format!("build_v2: task {id} (serial)"))?;
    let changed = git::changed_since(root, &pre).unwrap_or_default();
    let regen: BTreeSet<String> = changed.into_iter().filter(|f| b.is_lockfile(f)).collect();
    regenerate(b, &regen)?;
    let build = b.spec.build_only();
    let green = build.is_empty() || gate::check(b.runner, &b.run, &build, &b.baseline)?.green();
    if green {
        o.status = "serial".into();
    } else {
        git::reset_hard(root, &pre)?;
        o.status = "failed".into();
        o.note = "the build failed after the serial re-implementation too; left out".into();
    }
    save_json(b.host, &key, &format!("task {id}: {}", o.status), &o);
    Ok(o)
}

/// The build after a merge, with integrator rounds while it fails. `Ok(false)`: still failing.
fn build_green(b: &Build<'_>, task: &Task, o: &mut TaskOutcome) -> Result<bool, CrewError> {
    let build = b.spec.build_only();
    if build.is_empty() {
        return Ok(true);
    }
    for round in 0..=b.cfg.max_integrate_rounds {
        let c = gate::check(b.runner, &b.run, &build, &b.baseline)?;
        if c.green() {
            return Ok(true);
        }
        if round == b.cfg.max_integrate_rounds {
            break;
        }
        let fixed = soft(b.runner.step(step(
            format!("b2:integrate:{}:fix{}", task.id, round + 1),
            "integrator",
            format!("integrate {}: build fix {}", task.id, round + 1),
            format!(
                "## Integrate: the build fails after merging task {id}\nGoal: {goal}\nTask `{id}`: {tgoal}\n\n```\n{evidence}\n```\n\n\
                 Fix the integration in your worktree with the smallest change that keeps every task's intent. Do not \
                 commit; say what you changed.",
                id = task.id,
                goal = b.goal,
                tgoal = task.goal,
                evidence = tail_chars(&c.evidence, FAIL_CHARS),
            ),
            None,
            None,
        )))?;
        if let Err(e) = fixed {
            // The serial fallback starts from before the merge; nothing here is kept.
            b.event(format!("task {}: integrator round {} gave no answer ({e})", task.id, round + 1));
            break;
        }
        git::commit(&b.run.root, &format!("build_v2: integrate {} (fix {})", task.id, round + 1))?;
        o.note = format!("integrator rounds: {}", round + 1);
    }
    Ok(false)
}

/// Regenerates lockfiles mechanically (never merged by an agent) and commits them.
fn regenerate(b: &Build<'_>, files: &BTreeSet<String>) -> Result<(), CrewError> {
    for f in files {
        let Some(rule) = b.locks.iter().find(|l| l.file.eq_ignore_ascii_case(f)) else { continue };
        let r = b.runner.shell(&rule.regen)?;
        if r.ok {
            git::commit(&b.run.root, &format!("build_v2: regenerate {f}"))?;
            b.event(format!("{f} regenerated with `{}`", rule.regen));
        } else {
            b.event(format!("{f}: `{}` failed; the lockfile kept the build branch's side", rule.regen));
        }
    }
    Ok(())
}

// -------------------------------------------------------------------- 5-7 verify, review, fix

type AcceptFail = (String, String);

fn verify_review_fix(
    b: &Build<'_>,
    plan: &BuildPlan,
    outcomes: &BTreeMap<String, TaskOutcome>,
) -> Result<(GateCheck, Vec<AcceptFail>, BTreeMap<Lens, Vec<Finding>>), CrewError> {
    let mut findings: BTreeMap<Lens, Vec<Finding>> = BTreeMap::new();
    let mut lenses: Vec<Lens> = Lens::ALL.to_vec();
    let mut allowed = b.cfg.max_fix_rounds;
    let mut last: Option<BTreeSet<String>> = None;
    let mut guidance: Option<String> = None;
    let mut replanned_at: Option<u32> = None;
    let mut round = 0u32;
    loop {
        b.host.check()?;
        b.stage("verify", &format!("build 5/8 verify (round {round})"));
        let check = if b.spec.is_empty() { GateCheck::default() } else { gate::check(b.runner, &b.run, &b.spec, &b.baseline)? };
        let mut accept_fails: Vec<AcceptFail> = Vec::new();
        for t in &plan.tasks {
            let live = outcomes.get(&t.id).is_some_and(|o| o.status == "done" || o.status == "serial");
            if let (true, Some(cmd)) = (live, &t.accept_cmd) {
                let (ok, _, out) = gate::accept(b.runner, &b.run, cmd)?;
                if !ok {
                    accept_fails.push((t.id.clone(), out));
                }
            }
        }
        save_json(b.host, &format!("b2:verify:{round}"), &format!("verify {round}: {} new failures", check.new_failures.len()), &check);

        b.stage("review", &format!("build 6/8 review (round {round})"));
        let diff = b.run.diff_text()?;
        if !diff.trim().is_empty() && !lenses.is_empty() {
            for (lens, f) in review_diff(b.runner, b.goal, &diff, &lenses, &format!("b2:review:{round}"))? {
                findings.insert(lens, f);
            }
        }
        let blocking: Vec<&Finding> = findings.values().flatten().filter(|f| f.severity.blocks()).collect();
        if check.green() && accept_fails.is_empty() && blocking.is_empty() {
            return Ok((check, accept_fails, findings));
        }
        if !b.within_wall(&format!("fix round {}", round + 1))? {
            return Ok((check, accept_fails, findings));
        }
        if round >= allowed {
            let summary = format!("build run used its {allowed} fix rounds with problems left; allow two more?");
            let detail = problems(&check, &accept_fails, &blocking);
            if b.host.ask_user(&summary, &detail)? {
                allowed += 2;
            } else {
                return Ok((check, accept_fails, findings));
            }
        }
        round += 1;
        let mut signature: BTreeSet<String> = check.new_failures.iter().cloned().collect();
        signature.extend(accept_fails.iter().map(|(id, _)| format!("accept:{id}")));
        signature.extend(blocking.iter().map(|f| f.key()));
        let problems_text = problems(&check, &accept_fails, &blocking);
        // The same failure set two rounds running: the lead replans; stuck again after that, the
        // user decides.
        if last.as_ref() == Some(&signature) {
            if replanned_at.is_some() {
                let go_on = b.host.ask_user(
                    "build run's fix loop is stuck on the same failures after a replan; keep trying?",
                    &problems_text,
                )?;
                if !go_on {
                    return Ok((check, accept_fails, findings));
                }
            } else {
                let rp = soft(b.runner.step(step(
                    format!("b2:fixplan:{round}"),
                    "lead",
                    format!("replan fix {round}"),
                    format!(
                        "## Replan the fix\nGoal: {goal}\nThe fix loop has failed the same way two rounds running:\n{problems_text}\n\
                         Read the code and write revised fix instructions: what to change, where, and why the last \
                         attempt failed.",
                        goal = b.goal
                    ),
                    Some(&LOOK[..]),
                    None,
                )))?;
                replanned_at = Some(round);
                match rp {
                    Ok(rp) => {
                        guidance = Some(rp.text);
                        b.event(format!("fix round {round}: stalled on the same failures; the lead replanned the fix"));
                    }
                    Err(e) => b.event(format!("fix round {round}: stalled, and the lead's replan gave no answer ({e})")),
                }
            }
        }
        last = Some(signature);
        b.stage("fix", &format!("build 7/8 fix (round {round})"));
        let mut prompt = format!(
            "## Fix (round {round})\nGoal: {goal}\n\nOn the build branch (your worktree), fix what blocks delivery:\n{problems_text}\n\
             Already failing before this build (not yours to fix): {failing}\n",
            goal = b.goal,
            failing = b.known_failing,
        );
        if let Some(g) = &guidance {
            prompt.push_str(&format!("\nThe lead's instructions:\n{g}\n"));
        }
        prompt.push_str("\nFix each item or say why it is not a problem. Finish with what you changed and how you checked it.");
        if let Err(e) = soft(b.runner.step(step(format!("b2:fix:{round}"), "engineer", format!("fix {round}"), prompt, None, None)))? {
            // What the fix step left half-done stays uncommitted on the branch for the user to see.
            b.event(format!("fix round {round} gave no answer ({e}); delivering what there is"));
            return Ok((check, accept_fails, findings));
        }
        git::commit(&b.run.root, &format!("build_v2: fix round {round}"))?;
        // Re-review the lenses that had something blocking; the others keep their findings.
        lenses = Lens::ALL.into_iter().filter(|l| findings.get(l).is_some_and(|f| f.iter().any(|x| x.severity.blocks()))).collect();
    }
}

fn problems(check: &GateCheck, accept: &[AcceptFail], blocking: &[&Finding]) -> String {
    let mut s = String::new();
    if !check.new_failures.is_empty() {
        s.push_str(&format!(
            "- New failures against the baseline: {}\n```\n{}\n```\n",
            check.new_failures.join(", "),
            tail_chars(&check.evidence, FAIL_CHARS)
        ));
    }
    for (id, out) in accept {
        s.push_str(&format!("- Acceptance test of task {id} fails:\n```\n{}\n```\n", tail_chars(out, 2_000)));
    }
    for f in blocking {
        s.push_str(&format!("- {}", f.line()));
        if !f.evidence.is_empty() {
            s.push_str(&format!(" (evidence: {})", f.evidence));
        }
        if !f.fix.is_empty() {
            s.push_str(&format!(" Fix: {}", f.fix));
        }
        s.push('\n');
    }
    s
}

// -------------------------------------------------------------------- 8 deliver

#[allow(clippy::too_many_arguments)]
fn deliver(
    b: &Build<'_>,
    plan: &BuildPlan,
    outcomes: &BTreeMap<String, TaskOutcome>,
    check: Option<&GateCheck>,
    accept_fails: &[AcceptFail],
    findings: &BTreeMap<Lens, Vec<Finding>>,
    stopped: bool,
    files: [Option<String>; 2],
) -> Result<Dispatch, CrewError> {
    let diff = b.run.diff_summary()?;
    let all: Vec<&Finding> = findings.values().flatten().collect();
    let blocking = all.iter().filter(|f| f.severity.blocks()).count();
    let green = check.is_some_and(|c| c.green()) && accept_fails.is_empty();
    let done = outcomes.values().filter(|o| o.status == "done" || o.status == "serial").count();

    let mut s = String::new();
    s.push_str(&format!("build_v2: {done} of {} tasks delivered", plan.tasks.len()));
    s.push_str(if stopped { "; stopped early at the wall-clock budget.\n\n" } else { ".\n\n" });
    s.push_str("| task | status | rounds | note |\n|---|---|---|---|\n");
    for t in &plan.tasks {
        match outcomes.get(&t.id) {
            Some(o) => s.push_str(&format!("| {} | {} | {} | {} |\n", o.id, o.status, o.rounds, o.note.replace('|', "/"))),
            None => s.push_str(&format!("| {} | not run | 0 | |\n", t.id)),
        }
    }
    s.push_str("\nGate vs baseline: ");
    match check {
        Some(c) if c.green() => s.push_str("no new failures"),
        Some(c) => s.push_str(&format!("NEW FAILURES {}", c.new_failures.join(", "))),
        None => s.push_str("not run"),
    }
    s.push_str(&format!(
        " (baseline: {} failing, {} flaky",
        b.baseline.failing.len(),
        b.baseline.flaky.len()
    ));
    if let Some(c) = check {
        if !c.fixed.is_empty() {
            s.push_str(&format!("; now passing: {}", c.fixed.join(", ")));
        }
        if !c.flaky.is_empty() {
            s.push_str(&format!("; flaky this run: {}", c.flaky.join(", ")));
        }
    }
    s.push_str(").");
    if !accept_fails.is_empty() {
        s.push_str(&format!(" Acceptance tests failing: {}.", accept_fails.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    s.push_str("\n\nReview: ");
    let lens_line: Vec<String> = Lens::ALL
        .iter()
        .map(|l| {
            let f = findings.get(l).map(Vec::as_slice).unwrap_or(&[]);
            let block = f.iter().filter(|x| x.severity.blocks()).count();
            format!("{} {} finding(s), {} blocking", l.name(), f.len(), block)
        })
        .collect();
    s.push_str(&lens_line.join("; "));
    s.push('.');
    for f in all.iter().filter(|f| f.severity.blocks()).take(10) {
        s.push_str(&format!("\n- {}", f.line()));
    }
    if let Some(ci) = b.host.ci_status() {
        s.push_str(&format!("\nCI: {ci}"));
    }
    let events = b.events.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !events.is_empty() {
        s.push_str("\n\nAlong the way:");
        for e in events.iter().take(12) {
            s.push_str(&format!("\n- {e}"));
        }
    }
    if diff.files.is_empty() {
        s.push_str("\n\nThe build branch has no changes, so there is nothing to merge.");
    } else {
        s.push_str(&format!(
            "\n\n{} file{} changed, +{} −{} on `{}`; it needs a merge decision.",
            diff.files.len(),
            if diff.files.len() == 1 { "" } else { "s" },
            diff.insertions,
            diff.deletions,
            diff.branch
        ));
    }

    let mut artifacts: Vec<String> = files.into_iter().flatten().collect();
    artifacts.extend(write_artifact(b.host, "findings.json", &all));
    if let Some(c) = check {
        artifacts.extend(write_artifact(b.host, "gate.json", c));
    }
    artifacts.extend(write_artifact(b.host, "tasks.json", outcomes));
    artifacts.extend(diff.files.iter().map(|f| b.run.root.join(f).display().to_string()));

    let confidence = if diff.files.is_empty() || !green {
        Confidence::Low
    } else if blocking == 0 && done == plan.tasks.len() && !stopped {
        Confidence::High
    } else {
        Confidence::Medium
    };
    b.host.remember(&format!(
        "Build \"{}\": {done} of {} tasks delivered; gate {}; {blocking} blocking review findings left.",
        b.goal.lines().next().unwrap_or(""),
        plan.tasks.len(),
        if green { "green against the baseline" } else { "not green" }
    ));
    Ok(Dispatch { summary: s, artifacts, confidence, citations: Vec::new(), diff: Some(diff) })
}
