//! Non-negotiable 5 by construction: `marlowe_os::child` is the only way the harness starts a child
//! process (scrubbed environment, Job Object). This test reads every crate's `src/` and fails when a
//! file that uses `std::process` calls `.spawn()`, `.output()` or `.status()`, or calls the unscrubbed
//! `spawn_in_job` / `spawn_in_job_with_flags` anywhere, more often than its allowlist entry says. A
//! new direct spawn either goes through `marlowe_os::child` or is added here with its reason, in
//! review.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// (file under `crates/`, direct spawn calls allowed, why).
const ALLOWED: &[(&str, usize, &str)] = &[
    ("marlowe-os/src/child.rs", usize::MAX, "the spawner itself"),
    ("marlowe-os/src/job.rs", usize::MAX, "Job Object internals used by the spawner"),
    ("marlowe-os/src/tests.rs", usize::MAX, "Job Object tests"),
    ("marlowe-crew/src/playbook/agent.rs", 2, "scrubbed + spawn_in_job_with_flags by hand, with a bare fallback; seam-crew moves it to child::spawn"),
    ("marlowe-crew/src/sandbox.rs", 1, "scrubbed git_cmd; seam-crew moves it to child::run"),
    ("marlowe-crew/src/playbook/tests.rs", 1, "test fixture git"),
    ("marlowe-mcp/src/lib.rs", 2, "MCP stdio server (one is a source-text test); track K moves it to child::spawn"),
    ("marlowe-provider/src/ollama_runtime.rs", 5, "Ollama must outlive Marlowe; scrubbed by hand; Containment::Detached is its target"),
    ("marlowe-sentinel/src/git.rs", 2, "cfg(test) fixtures only; the probe itself uses child::spawn"),
    ("marlowe-typeset/src/helper.rs", 1, "env_clear helper, stricter than the scrub; ChildSpec::with_clear_env is its target"),
    ("marlowe/src/daemon_ctl.rs", 1, "relaunches marlowe itself, which needs the key environment fallback"),
    ("marlowe/src/launcher.rs", 1, "opens the user's terminal running marlowe"),
    ("marlowe-voice/src/tests.rs", 3, "`voice.status()` calls, not processes (the file names std::process for a pid)"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn direct_spawns(text: &str) -> usize {
    let code = || text.lines().filter(|l| !l.trim_start().starts_with("//"));
    // The job spawners skip the scrub, so they count wherever they are called.
    let job = code().map(|l| l.matches("spawn_in_job(").count() + l.matches("spawn_in_job_with_flags(").count()).sum::<usize>();
    if !(text.contains("process::") || text.contains("Command::new") || text.contains("Command}")) {
        return job;
    }
    job + code()
        .map(|l| l.matches(".spawn()").count() + l.matches(".output()").count() + l.matches(".status()").count())
        .sum::<usize>()
}

#[test]
fn the_audit_counts_the_unscrubbed_job_spawners() {
    assert_eq!(direct_spawns("let c = marlowe_os::spawn_in_job(&mut cmd)?;"), 1);
    assert_eq!(direct_spawns("use marlowe_os::spawn_in_job_with_flags;
let c = spawn_in_job_with_flags(&mut cmd, 0)?;"), 1);
    assert_eq!(direct_spawns("// spawn_in_job(&mut cmd)"), 0);
    assert_eq!(direct_spawns("std::process::Command::new(\"x\").status()"), 1);
}

#[test]
fn every_child_process_starts_through_marlowe_os_child() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let allowed: BTreeMap<&str, usize> = ALLOWED.iter().map(|(f, n, _)| (*f, *n)).collect();
    let mut offenders = Vec::new();
    let mut scanned = 0;
    for krate in std::fs::read_dir(&crates).unwrap().flatten() {
        let mut files = Vec::new();
        rust_files(&krate.path().join("src"), &mut files);
        for f in files {
            scanned += 1;
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            let n = direct_spawns(&text);
            let rel = f.strip_prefix(&crates).unwrap().to_string_lossy().replace('\\', "/");
            let limit = allowed.get(rel.as_str()).copied().unwrap_or(0);
            if n > limit {
                offenders.push(format!("{rel}: {n} direct spawn calls (allowed {limit})"));
            }
        }
    }
    assert!(scanned > 100, "the audit read the workspace ({scanned} files)");
    assert!(
        offenders.is_empty(),
        "start children through marlowe_os::child (scrubbed env, Job Object), not std::process directly:\n{}",
        offenders.join("\n")
    );
}
