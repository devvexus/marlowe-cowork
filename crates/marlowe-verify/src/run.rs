//! Running the profile's commands and the gate (format, lint, typecheck, build, test).
//!
//! A run is a [`StepReport`]: exit status, how long, the failing test ids and compiler findings
//! read from the output ([`crate::output`]), and a bounded excerpt that keeps the failures. When
//! the excerpt left lines out the full log is on disk and the report names it. The gate returns
//! one report per step and, given a baseline of already-failing ids, which failures are new.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cmd::Cmd;
use crate::env::Env;
use crate::output::{self, Diagnostic};
use crate::profile::{Kind, ProjectCmd, ProjectProfile};

/// Bytes of excerpt per step in a tool result.
pub const EXCERPT_BYTES: usize = 12_000;
/// Full logs kept on disk.
const KEEP_LOGS: usize = 24;

/// Where full logs go when an excerpt left lines out.
#[derive(Debug, Default)]
pub struct LogStore {
    dir: Mutex<Option<PathBuf>>,
}

impl LogStore {
    pub fn set_dir(&self, dir: PathBuf) {
        *self.dir.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir);
    }

    fn dir(&self) -> PathBuf {
        let mut g = self.dir.lock().unwrap_or_else(|p| p.into_inner());
        g.get_or_insert_with(|| std::env::temp_dir().join("marlowe-verify").join("logs")).clone()
    }

    /// Writes `text` as a new log named after `label` and prunes old ones. `None` if the disk
    /// refuses (the excerpt still stands).
    pub fn save(&self, label: &str, text: &str) -> Option<PathBuf> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = self.dir();
        std::fs::create_dir_all(&dir).ok()?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name: String = label.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(40).collect();
        let path = dir.join(format!("{stamp}-{seq}-{name}.log"));
        std::fs::write(&path, text).ok()?;
        prune(&dir, KEEP_LOGS);
        Some(path)
    }
}

/// Deletes all but the newest `keep` files in `dir`.
pub fn prune(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// What one command did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReport {
    pub kind: Kind,
    /// The command line that ran (or would have).
    pub command: String,
    pub cwd: String,
    pub ok: bool,
    pub exit: i32,
    pub timed_out: bool,
    pub cancelled: bool,
    pub flooded: bool,
    pub duration_ms: u64,
    pub tally: Option<String>,
    pub failing: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub excerpt: String,
    pub truncated: bool,
    pub log_path: Option<PathBuf>,
    pub log_lines: usize,
    /// Why nothing ran (the program is missing, the directory is outside the workspace).
    pub not_run: Option<String>,
}

impl StepReport {
    fn not_run(pc: &ProjectCmd, why: String) -> Self {
        Self {
            kind: pc.kind,
            command: pc.cmd.display(),
            cwd: pc.cwd.clone(),
            ok: false,
            exit: -1,
            timed_out: false,
            cancelled: false,
            flooded: false,
            duration_ms: 0,
            tally: None,
            failing: Vec::new(),
            diagnostics: Vec::new(),
            excerpt: String::new(),
            truncated: false,
            log_path: None,
            log_lines: 0,
            not_run: Some(why),
        }
    }

    /// Whether a missing tool should not fail a gate (`lint`, `format`, `typecheck` only).
    fn optional_kind(&self) -> bool {
        matches!(self.kind, Kind::Lint | Kind::Format | Kind::Typecheck)
    }

    /// The report as the model reads it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(why) = &self.not_run {
            out.push_str(&format!("{} NOT RUN: `{}` ({why})\n", self.kind.name(), self.command));
            return out;
        }
        let verdict = if self.ok {
            "ok"
        } else if self.timed_out {
            "TIMED OUT"
        } else if self.cancelled {
            "INTERRUPTED"
        } else if self.flooded {
            "OUTPUT CAP"
        } else {
            "FAILED"
        };
        let secs = self.duration_ms as f64 / 1000.0;
        out.push_str(&format!("{} {verdict}: `{}`{} (exit {}, {secs:.1} s)\n", self.kind.name(), self.command, if self.cwd != "." { format!(" in {}", self.cwd) } else { String::new() }, self.exit));
        if let Some(t) = &self.tally {
            out.push_str(&format!("tests: {t}\n"));
        }
        if !self.failing.is_empty() {
            let shown: Vec<&str> = self.failing.iter().take(30).map(String::as_str).collect();
            out.push_str(&format!("failing ({}): {}{}\n", self.failing.len(), shown.join(", "), if self.failing.len() > 30 { ", ..." } else { "" }));
        }
        if !self.diagnostics.is_empty() {
            out.push_str(&format!("findings ({}):\n", self.diagnostics.len()));
            for d in self.diagnostics.iter().take(15) {
                out.push_str(&format!("  {}\n", d.render()));
            }
        }
        if !self.excerpt.trim().is_empty() && (!self.ok || self.kind == Kind::Test) {
            out.push_str("output:\n");
            out.push_str(self.excerpt.trim_end());
            out.push('\n');
        }
        if let Some(p) = &self.log_path {
            out.push_str(&format!("[full output: {}, {} lines]\n", p.display(), self.log_lines));
        }
        out
    }
}

/// A command's raw outcome.
struct Raw {
    code: i32,
    timed_out: bool,
    flooded: bool,
    cancelled: bool,
    text: String,
    stdout: String,
    elapsed: Duration,
}

fn exec(env: &Env<'_>, cmd: &Cmd, cwd: &Path, timeout: Duration, env_allow: &[String], extra_env: &[(String, String)]) -> Result<Raw, String> {
    let req = env.request(cmd, cwd, timeout, env_allow, extra_env)?;
    let started = Instant::now();
    let out = env.ws.exec(&req, &|| env.cancel.is_cancelled()).map_err(|e| format!("could not start `{}`: {e}", cmd.program()))?;
    let (so, se) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    let text = match (se.trim().is_empty(), so.trim().is_empty()) {
        (true, _) => so.clone(),
        (false, true) => se,
        (false, false) => format!("{}\n{}", se.trim_end(), so),
    };
    Ok(Raw { code: out.code, timed_out: out.timed_out, flooded: out.flooded, cancelled: out.cancelled, text, stdout: so, elapsed: started.elapsed() })
}

/// Runs one profile command (its `pre` step first when its marker is missing) with `extra`
/// arguments appended.
pub fn run_project_cmd(env: &Env<'_>, pc: &ProjectCmd, extra: &[String], logs: &LogStore) -> StepReport {
    run_inner(env, pc, extra, &[], logs)
}

/// A one-off command reported like a profile command: `label` names the step.
pub fn run_custom(env: &Env<'_>, label: Kind, cmd: &Cmd, cwd: &str, timeout: Duration, env_extra: &[(String, String)], logs: &LogStore) -> StepReport {
    let mut pc = ProjectCmd::new(label, crate::profile::Stack::Make, cwd, &[]);
    pc.stack = None;
    pc.cmd = cmd.clone();
    pc.timeout_s = Some(timeout.as_secs().max(1));
    run_inner(env, &pc, &[], env_extra, logs)
}

fn run_inner(env: &Env<'_>, pc: &ProjectCmd, extra: &[String], env_extra: &[(String, String)], logs: &LogStore) -> StepReport {
    let cwd = match env.cwd(&pc.cwd) {
        Ok(c) => c,
        Err(e) => return StepReport::not_run(pc, e),
    };
    let mut cmd = pc.cmd.clone();
    if !extra.is_empty() {
        if let Some(sep) = &pc.args_separator {
            if !cmd.argv.iter().any(|a| a == sep) {
                cmd.argv.push(sep.clone());
            }
        }
        cmd.argv.extend(extra.iter().cloned());
    }
    let mut preface = String::new();
    if let Some((pre, marker)) = &pc.pre {
        if !cwd.join(marker).exists() {
            match exec(env, pre, &cwd, pc.timeout(), &[], env_extra) {
                Err(e) => return StepReport::not_run(pc, e),
                Ok(r) if r.code != 0 || r.timed_out || r.cancelled => {
                    let mut rep = finish(pc, &pre.display(), r, logs);
                    rep.kind = pc.kind;
                    return rep;
                }
                Ok(r) => preface = format!("[first ran `{}` ({:.1} s)]\n", pre.display(), r.elapsed.as_secs_f64()),
            }
        }
    }
    match exec(env, &cmd, &cwd, pc.timeout(), &[], env_extra) {
        Err(e) => StepReport::not_run(pc, e),
        Ok(r) => {
            let mut rep = finish(pc, &cmd.display(), r, logs);
            if !preface.is_empty() {
                rep.excerpt = format!("{preface}{}", rep.excerpt);
            }
            rep
        }
    }
}

fn finish(pc: &ProjectCmd, command: &str, r: Raw, logs: &LogStore) -> StepReport {
    let listed = pc.fail_on_output && r.code == 0 && !r.stdout.trim().is_empty();
    let ok = r.code == 0 && !r.timed_out && !r.flooded && !r.cancelled && !listed;
    let parsed = output::parse(&r.text, pc.kind == Kind::Test);
    let failing = if listed { output::files_listed(&r.stdout) } else { parsed.failing };
    let (excerpt, truncated) = output::condense(&r.text, EXCERPT_BYTES);
    let log_lines = r.text.lines().count();
    let log_path = if truncated { logs.save(&format!("{}-{}", pc.kind.name(), pc.cmd.program_name()), &r.text) } else { None };
    StepReport {
        kind: pc.kind,
        command: command.to_string(),
        cwd: pc.cwd.clone(),
        ok,
        exit: r.code,
        timed_out: r.timed_out,
        cancelled: r.cancelled,
        flooded: r.flooded,
        duration_ms: r.elapsed.as_millis() as u64,
        tally: parsed.tally,
        failing,
        diagnostics: parsed.diagnostics,
        excerpt,
        truncated,
        log_path,
        log_lines,
        not_run: None,
    }
}

/// What the gate does.
#[derive(Debug, Clone, Default)]
pub struct GateOpts {
    /// Kinds to run, in gate order (default: all five).
    pub kinds: Option<Vec<Kind>>,
    /// Run every step even after one fails (baselines want this).
    pub keep_going: bool,
    /// Ids that were already failing before the change.
    pub baseline: Option<Vec<String>>,
}

/// The gate's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    pub steps: Vec<StepReport>,
    /// Kinds with no command in the profile.
    pub absent: Vec<Kind>,
    pub ok: bool,
    /// Failing ids not in the baseline, when one was given.
    pub new_failures: Option<Vec<String>>,
    /// Baseline ids that no longer fail.
    pub fixed: Option<Vec<String>>,
    /// The run stopped after a failing step.
    pub stopped_early: bool,
}

/// Runs the gate over `profile`.
pub fn gate(env: &Env<'_>, profile: &ProjectProfile, opts: &GateOpts, logs: &LogStore) -> GateReport {
    let kinds: Vec<Kind> = match &opts.kinds {
        Some(k) => Kind::GATE.into_iter().filter(|g| k.contains(g)).collect(),
        None => Kind::GATE.to_vec(),
    };
    let mut steps = Vec::new();
    let mut absent = Vec::new();
    let mut stopped_early = false;
    'kinds: for kind in kinds {
        let cmds = profile.commands_of(kind);
        if cmds.is_empty() {
            absent.push(kind);
            continue;
        }
        for pc in cmds {
            if env.cancel.is_cancelled() {
                stopped_early = true;
                break 'kinds;
            }
            let rep = run_project_cmd(env, pc, &[], logs);
            let failed = !rep.ok && !(rep.not_run.is_some() && rep.optional_kind());
            steps.push(rep);
            if failed && !opts.keep_going {
                stopped_early = true;
                break 'kinds;
            }
        }
    }
    let ok = !stopped_early && steps.iter().all(|s| s.ok || (s.not_run.is_some() && s.optional_kind())) && !steps.is_empty();
    let failing: std::collections::BTreeSet<&String> = steps.iter().flat_map(|s| s.failing.iter()).collect();
    let (new_failures, fixed) = match &opts.baseline {
        Some(base) => {
            let base_set: std::collections::BTreeSet<&String> = base.iter().collect();
            (
                Some(failing.iter().filter(|f| !base_set.contains(*f)).map(|s| s.to_string()).collect()),
                Some(base_set.iter().filter(|b| !failing.contains(*b)).map(|s| s.to_string()).collect()),
            )
        }
        None => (None, None),
    };
    GateReport { steps, absent, ok, new_failures, fixed, stopped_early }
}

impl GateReport {
    /// The gate's verdict against a baseline: no step failed beyond what already failed.
    pub fn ok_vs_baseline(&self) -> bool {
        match &self.new_failures {
            Some(new) => new.is_empty() && self.steps.iter().all(|s| s.ok || !s.failing.is_empty() || (s.not_run.is_some() && s.optional_kind())) && !self.steps.is_empty(),
            None => self.ok,
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let line = |s: &StepReport| -> String {
            let mark = if s.not_run.is_some() { "skip" } else if s.ok { "ok" } else { "FAIL" };
            format!("  {mark:<4} {:<9} {} ({:.1} s)", s.kind.name(), s.command, s.duration_ms as f64 / 1000.0)
        };
        let verdict = if self.ok {
            "gate PASSED"
        } else if self.new_failures.is_some() && self.ok_vs_baseline() {
            "gate: no NEW failures against the baseline"
        } else {
            "gate FAILED"
        };
        out.push_str(&format!("{verdict}\n"));
        for s in &self.steps {
            out.push_str(&line(s));
            out.push('\n');
        }
        if !self.absent.is_empty() {
            let names: Vec<&str> = self.absent.iter().map(|k| k.name()).collect();
            out.push_str(&format!("  (no command for: {})\n", names.join(", ")));
        }
        if self.stopped_early {
            out.push_str("  (stopped at the first failure; keep_going:true runs every step)\n");
        }
        if let Some(new) = &self.new_failures {
            out.push_str(&format!("new failures vs baseline: {}\n", if new.is_empty() { "none".to_string() } else { new.join(", ") }));
        }
        if let Some(fixed) = &self.fixed {
            if !fixed.is_empty() {
                out.push_str(&format!("baseline failures now passing: {}\n", fixed.join(", ")));
            }
        }
        out.push('\n');
        for s in self.steps.iter().filter(|s| !s.ok) {
            out.push_str(&s.render());
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Source, Stack};

    fn pc(kind: Kind, argv: &[&str]) -> ProjectCmd {
        let mut c = ProjectCmd::new(kind, Stack::Pip, ".", argv);
        c.source = Source::Override;
        c
    }

    fn python() -> Option<String> {
        crate::cmd::resolve_program("python", None).map(|p| p.to_string_lossy().into_owned())
    }

    fn root() -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-verify-run-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_passing_and_a_failing_command_report_exit_tally_and_failing_ids() {
        let Some(py) = python() else { return };
        let root = root();
        let cancel = marlowe_model::CancelToken::new();
        let ws = marlowe_os::workspace::LocalFs;
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let logs = LogStore::default();
        logs.set_dir(root.join("logs"));
        let ok = run_project_cmd(&env, &pc(Kind::Build, &[py.as_str(), "-c", "print('built')"]), &[], &logs);
        assert!(ok.ok && ok.exit == 0 && ok.not_run.is_none(), "{ok:?}");
        let script = "print('FAILED tests/test_a.py::test_x - boom')\nprint('=== 1 failed, 2 passed in 0.10s ===')\nraise SystemExit(1)";
        let bad = run_project_cmd(&env, &pc(Kind::Test, &[py.as_str(), "-c", script]), &[], &logs);
        assert!(!bad.ok && bad.exit == 1);
        assert_eq!(bad.failing, ["tests/test_a.py::test_x"]);
        assert_eq!(bad.tally.as_deref(), Some("1 failed, 2 passed in 0.10s"));
        assert!(bad.render().contains("test FAILED") && bad.render().contains("failing (1): tests/test_a.py::test_x"), "{}", bad.render());
        let missing = run_project_cmd(&env, &pc(Kind::Lint, &["no-such-tool-xyz"]), &[], &logs);
        assert!(missing.not_run.as_deref().unwrap().contains("not on PATH") && !missing.ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_timeout_kills_the_command_and_is_reported() {
        let Some(py) = python() else { return };
        let root = root();
        let cancel = marlowe_model::CancelToken::new();
        let ws = marlowe_os::workspace::LocalFs;
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let mut c = pc(Kind::Test, &[py.as_str(), "-c", "import time; time.sleep(30)"]);
        c.timeout_s = Some(1);
        let r = run_project_cmd(&env, &c, &[], &LogStore::default());
        assert!(r.timed_out && !r.ok, "{r:?}");
        assert!(r.duration_ms < 15_000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_gate_stops_at_the_first_failure_and_diffs_against_a_baseline() {
        let Some(py) = python() else { return };
        let root = root();
        let cancel = marlowe_model::CancelToken::new();
        let ws = marlowe_os::workspace::LocalFs;
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let logs = LogStore::default();
        let mut profile = ProjectProfile::default();
        profile.commands.push(pc(Kind::Lint, &[py.as_str(), "-c", "print('lint ok')"]));
        profile.commands.push(pc(Kind::Build, &[py.as_str(), "-c", "print('built')"]));
        profile.commands.push(pc(Kind::Test, &[py.as_str(), "-c", "print('FAILED t::old - x')\nprint('FAILED t::new - y')\nraise SystemExit(1)"]));
        let g = gate(&env, &profile, &GateOpts { keep_going: true, baseline: Some(vec!["t::old".into(), "t::fixed".into()]), ..Default::default() }, &logs);
        assert_eq!(g.steps.len(), 3);
        assert!(!g.ok);
        assert_eq!(g.new_failures.as_deref(), Some(&["t::new".to_string()][..]));
        assert_eq!(g.fixed.as_deref(), Some(&["t::fixed".to_string()][..]));
        assert!(g.absent.contains(&Kind::Format) && g.absent.contains(&Kind::Typecheck));
        assert!(!g.ok_vs_baseline());
        let g = gate(&env, &profile, &GateOpts { baseline: Some(vec!["t::old".into(), "t::new".into()]), ..Default::default() }, &logs);
        assert!(g.ok_vs_baseline() && !g.ok, "only already-failing tests fail: {}", g.render());
        let g = gate(&env, &profile, &GateOpts::default(), &logs);
        assert_eq!(g.steps.len(), 3, "the test step is last; nothing before it failed");
        let mut failing_build = profile.clone();
        failing_build.commands[1] = pc(Kind::Build, &[py.as_str(), "-c", "raise SystemExit(2)"]);
        let g = gate(&env, &failing_build, &GateOpts::default(), &logs);
        assert!(g.stopped_early && g.steps.len() == 2 && g.render().contains("gate FAILED"), "{}", g.render());
        let _ = std::fs::remove_dir_all(&root);
    }
}
