//! `marlowe eval` (tracks L0 and L): the thin entry to the coding eval in `eval/coding/`
//! (design section 6). The harness itself is Python on the standard library; this finds the
//! checkout and an interpreter, hands over the arguments and the path of this binary, and
//! returns the harness's exit code. The LongMemEval adapter stays at `--eval-adapter`.
//!
//! | Command | What |
//! |---|---|
//! | `marlowe eval tools` | the tool names a local-profile session exposes under the present environment (the shell-only arm checks `MARLOWE_TOOL_GROUPS=shell` with it) |
//! | `marlowe eval where` | the checkout and interpreters this entry found |
//! | anything else | forwarded to `python -I eval/coding/run.py ...` with `MARLOWE_BIN` set (`help` lists the harness's commands) |
//!
//! A headless run writes its tool-call repair counts to the file named by `MARLOWE_EVAL_STATS`
//! when the variable is set ([`write_run_stats`]); the harness reads them for the report.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use marlowe_agent::ToolHost;
use marlowe_os::child::{self, ChildSpec};

/// Where the harness is, when the binary is not inside the checkout.
pub const DIR_ENV: &str = "MARLOWE_EVAL_DIR";
/// The interpreter to use.
pub const PYTHON_ENV: &str = "MARLOWE_PYTHON";
/// Set by a headless run's parent to get its repair counts written as JSON.
pub const STATS_ENV: &str = "MARLOWE_EVAL_STATS";

const ENTRY: &str = "run.py";

pub fn run(_home: &Path, args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("tools") => {
            println!("{}", exposed_tools().join("\n"));
            Ok(0)
        }
        Some("where") => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cwd = std::env::current_dir().unwrap_or_default();
            match find_eval_dir(&exe, &cwd, std::env::var_os(DIR_ENV)) {
                Some(d) => println!("eval dir: {}", d.display()),
                None => println!("eval dir: not found (set {DIR_ENV} to the eval/coding folder)"),
            }
            let pythons: Vec<String> = python_candidates(std::env::var_os(PYTHON_ENV)).iter().map(Python::describe).collect();
            println!("python:   {}", pythons.join(" | "));
            Ok(0)
        }
        _ => forward(args),
    }
}

/// The tools a local-profile session would expose with the groups the environment selects.
fn exposed_tools() -> Vec<String> {
    let profile = marlowe_agent::ExposureProfile::Local;
    let groups = marlowe_exec::specs::exposed_groups(profile, None);
    let host = marlowe_exec::BuiltinTools::new(None).with_exposure(profile, groups);
    host.specs().into_iter().map(|s| s.name).collect()
}

/// An interpreter and the arguments that come before the script.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Python {
    program: OsString,
    pre: Vec<OsString>,
}

impl Python {
    fn describe(&self) -> String {
        let mut parts = vec![self.program.to_string_lossy().into_owned()];
        parts.extend(self.pre.iter().map(|a| a.to_string_lossy().into_owned()));
        parts.join(" ")
    }
}

/// The interpreters to try, in order: the override, then `python`, then the `py` launcher.
fn python_candidates(over: Option<OsString>) -> Vec<Python> {
    let mut out = Vec::new();
    if let Some(p) = over.filter(|p| !p.is_empty()) {
        out.push(Python { program: p, pre: Vec::new() });
    }
    out.push(Python { program: "python".into(), pre: Vec::new() });
    out.push(Python { program: "py".into(), pre: vec!["-3".into()] });
    out
}

/// The `eval/coding` folder: the override, else the nearest one above the binary or the working
/// directory.
fn find_eval_dir(exe: &Path, cwd: &Path, over: Option<OsString>) -> Option<PathBuf> {
    let holds_entry = |d: &Path| d.join(ENTRY).is_file();
    if let Some(d) = over.map(PathBuf::from).filter(|d| !d.as_os_str().is_empty()) {
        return holds_entry(&d).then_some(d);
    }
    let starts = [exe.parent().map(Path::to_path_buf), Some(cwd.to_path_buf())];
    for start in starts.into_iter().flatten() {
        for dir in start.ancestors() {
            let candidate = dir.join("eval").join("coding");
            if holds_entry(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// `python -I <dir>/run.py <args>`; isolated mode ignores `PYTHON*` variables and user site
/// directories, so the harness runs the code in the checkout and nothing else.
fn command_for(py: &Python, dir: &Path, args: &[String], exe: &Path) -> Command {
    let mut cmd = Command::new(&py.program);
    cmd.args(&py.pre).arg("-I").arg(dir.join(ENTRY)).args(args).env("MARLOWE_BIN", exe);
    cmd
}

fn forward(args: &[String]) -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let dir = find_eval_dir(&exe, &cwd, std::env::var_os(DIR_ENV))
        .ok_or_else(|| format!("the coding eval (eval/coding/{ENTRY}) was not found next to this binary or above the working directory; set {DIR_ENV}"))?;
    let mut last = String::new();
    for py in python_candidates(std::env::var_os(PYTHON_ENV)) {
        let mut cmd = command_for(&py, &dir, args, &exe);
        match child::spawn(&mut cmd, &ChildSpec::new()) {
            Ok(mut proc) => {
                let status = proc.child_mut().wait().map_err(|e| e.to_string())?;
                return Ok(status.code().unwrap_or(1));
            }
            Err(e) => last = format!("{}: {e}", py.describe()),
        }
    }
    Err(format!("no Python interpreter could be started ({last}); set {PYTHON_ENV}"))
}

/// Writes this process's tool-call repair counts to `$MARLOWE_EVAL_STATS` as JSON, if set.
/// Best effort: a run never fails over its statistics.
pub fn write_run_stats() {
    let Some(path) = std::env::var_os(STATS_ENV).filter(|p| !p.is_empty()) else { return };
    let s = crate::wire::repair_stats().snapshot();
    let body = serde_json::json!({
        "calls": s.calls, "clean": s.clean, "repaired": s.repaired, "from_text": s.from_text,
        "unusable": s.unusable, "truncated": s.truncated, "invalid": s.invalid, "unknown_tool": s.unknown_tool,
        "valid_rate": s.valid_rate(), "repair_rate": s.repair_rate(), "fixes": s.fixes,
    });
    let _ = std::fs::write(path, body.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-eval-entry-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_checkout_is_found_above_the_binary_or_the_working_directory() {
        let root = scratch("find");
        let eval = root.join("eval").join("coding");
        std::fs::create_dir_all(&eval).unwrap();
        std::fs::write(eval.join(ENTRY), "").unwrap();
        let exe = root.join("target").join("debug").join("marlowe.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let elsewhere = scratch("elsewhere");
        assert_eq!(find_eval_dir(&exe, &elsewhere, None), Some(eval.clone()));
        assert_eq!(find_eval_dir(&elsewhere.join("marlowe.exe"), &root.join("crates"), None), Some(eval.clone()));
        assert_eq!(find_eval_dir(&elsewhere.join("marlowe.exe"), &elsewhere, None), None);
        // The override wins and must hold the entry script.
        assert_eq!(find_eval_dir(&elsewhere.join("marlowe.exe"), &elsewhere, Some(eval.clone().into_os_string())), Some(eval));
        assert_eq!(find_eval_dir(&exe, &elsewhere, Some(elsewhere.clone().into_os_string())), None);
    }

    #[test]
    fn interpreters_are_tried_override_first() {
        let c = python_candidates(Some("D:/py/python.exe".into()));
        assert_eq!(c[0].program, OsString::from("D:/py/python.exe"));
        assert_eq!(c.last().unwrap().pre, vec![OsString::from("-3")]);
        assert_eq!(python_candidates(None).len(), 2);
        assert_eq!(python_candidates(Some(OsString::new())).len(), 2);
    }

    #[test]
    fn the_command_isolates_python_and_passes_the_binary() {
        let py = Python { program: "python".into(), pre: Vec::new() };
        let cmd = command_for(&py, Path::new("C:/repo/eval/coding"), &["run".into(), "--arm".into(), "full".into()], Path::new("C:/bin/marlowe.exe"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(args, ["-I", "C:/repo/eval/coding/run.py", "run", "--arm", "full"]);
        assert!(cmd.get_envs().any(|(k, v)| k == "MARLOWE_BIN" && v.is_some()));
    }

    #[test]
    fn the_shell_only_group_list_exposes_the_shell_and_nothing_else() {
        let groups = marlowe_exec::specs::parse_group_list("shell").unwrap();
        let host = marlowe_exec::BuiltinTools::new(None).with_exposure(marlowe_agent::ExposureProfile::Local, groups);
        let names: Vec<String> = host.specs().into_iter().map(|s| s.name).collect();
        assert!(names.iter().any(|n| n == "bash"), "{names:?}");
        for banned in ["read", "write", "edit", "grep", "glob", "web_fetch"] {
            assert!(!names.iter().any(|n| n == banned), "{banned} must not be in the shell-only set: {names:?}");
        }
    }
}
