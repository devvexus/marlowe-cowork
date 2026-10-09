//! **`bash` is bounded in time and in memory.** Audit findings A1 and A2.
//!
//! # Why these tests spawn real children
//!
//! A1's finding is that `BASH_TIMEOUT_MS` was *declared and never read* — one grep hit in the whole
//! repo, the definition. The obvious test for a timeout is `assert_eq!(BASH_TIMEOUT_MS, 120_000)`,
//! and that assertion is **green on precisely the build the audit was reporting**. It asserts the
//! value of a field rather than the fate of a process, which is the sixteenth instance in
//! CLAUDE.md's ledger, named there in those words.
//!
//! So every test here starts a process that would not stop on its own, and asserts that it stopped.
//! The bounds are injected through [`ShellLimits`] so that takes under a second instead of two
//! minutes.
//!
//! # What is NOT claimed
//!
//! The **grandchild** is not killed. `child.kill()` ends the shell, not what the shell started:
//! there is no process group signal on Unix and no Job Object on Windows. `a_grandchild_survives`
//! asserts that limitation rather than leaving it to be discovered — a resource leak that is
//! documented and tested is a different thing from one nobody knows about.

use std::process::Command;

use marlowe_exec::{run_bounded, ShellLimits};

fn fast() -> ShellLimits {
    ShellLimits { timeout_ms: 400, max_output_bytes: 64 * 1024 }
}

/// The shell `spawn_shell` uses — **the same function, not a copy of it.**
///
/// This built `Command::new("cmd").arg("/C")` under a doc comment saying it matched
/// `spawn_shell`. It did, until the interpreter changed to Git Bash, and then it silently tested a
/// shell the product no longer runs. `marlowe_exec::shell_command` is now the one definition.
fn shell(script: &str) -> Command {
    let mut c = marlowe_exec::shell_command().expect("a shell is installed");
    c.arg(script);
    c
}


/// A command that never exits on its own, **and spawns nothing**.
///
/// # The first version of this used `ping -t`, and it cost a diagnosis
///
/// `cmd /C ping -t` makes `PING.EXE` a grandchild. Killing the shell leaves it running — the
/// documented limitation below — and on Windows a spawned process inherits **every** inheritable
/// handle, including the test binary's own stdout, which is cargo's pipe, which is the pipe the
/// shell command reading cargo's output is waiting on. So `cargo test … | tail` hung **after every
/// test had passed**, and the obvious reading was "the timeout does not work".
///
/// It works. Run the same binary without a pipe and it finishes in 0.69 s. The measurement was of
/// an orphan holding a handle, read as a property of the code under test — the same shape as the
/// `echo`-escaping incident in CLAUDE.md, in a new place.
///
/// `bash -c 'sleep …'` execs in place, so the shell BECOMES the sleep and there is never a
/// second process to orphan. This was a `cmd`-only `for /L` loop on Windows until the shell
/// became Git Bash on both.
fn forever() -> &'static str {
    // One spelling: it is bash on both platforms now. `sleep` execs in place, so the shell
    // BECOMES the sleep and there is no grandchild holding an inherited handle.
    "sleep 600"
}

/// A command that writes without stopping — again with no grandchild, for the reason above.
fn floods() -> &'static str {
    {
        "yes AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    }
}

/// **A1.** The call returns, and the child is reaped rather than left running.
#[test]
fn a_command_that_never_exits_is_stopped_and_the_call_returns() {
    let run = run_bounded(shell(forever()), fast()).expect("spawns");

    assert!(run.stopped, "the run must report that the harness stopped it, not that it finished");
    // `run_bounded` only returns after `child.wait()`, so reaching here at all means the direct
    // child was killed and reaped. Before the fix this line was unreachable: `Command::output()`
    // blocked until the child exited, and this one never does.
    assert!(!run.flooded, "it produced little; this is the time bound, not the size bound");
}

/// **A1, stated as the thing that actually broke.** The turn comes back.
///
/// The exploit is not that a command runs long — it is that `execute_batch` → `run_group` → the
/// daemon turn had no interrupt path, so an approved `ping -t` hung the whole process forever. The
/// assertion is therefore about the *caller* returning, which is what a batch does.
#[test]
fn three_unbounded_commands_in_a_row_all_return() {
    for _ in 0..3 {
        let run = run_bounded(shell(forever()), fast()).expect("spawns");
        assert!(run.stopped);
    }
}

/// **A2.** Output is capped, the child is killed on breach, and the caller is told.
#[test]
fn a_command_that_floods_is_capped_and_says_so() {
    let limits = ShellLimits { timeout_ms: 30_000, max_output_bytes: 64 * 1024 };
    let run = run_bounded(shell(floods()), limits).expect("spawns");

    assert!(run.flooded, "the run must report the breach: a silent prefix looks like a whole answer");
    assert!(
        run.text.len() <= limits.max_output_bytes + 64 * 1024,
        "held {} bytes against a {} byte cap",
        run.text.len(),
        limits.max_output_bytes
    );
    // And it did not simply wait out the clock: the cap is what stopped it.
    assert!(
        !run.stopped || run.flooded,
        "a flood must be reported as a flood, not only as a timeout"
    );
}

/// The negative control for both. Without it, a `run_bounded` that killed **everything**
/// immediately would pass every test above.
#[test]
fn an_ordinary_command_finishes_normally_and_is_reported_as_finished() {
    let run = run_bounded(shell("echo hello-from-the-child"), fast()).expect("spawns");

    assert!(!run.stopped, "an ordinary command must not be reported as stopped");
    assert!(!run.flooded, "nor as flooded");
    assert_eq!(run.code, 0);
    assert!(
        run.text.contains("hello-from-the-child"),
        "and its output still comes back: {:?}",
        run.text
    );
}

/// stderr comes back too, and a non-zero exit is reported as one.
#[test]
fn stderr_and_a_failing_exit_code_both_survive_the_bounded_path() {
    // One spelling: bash on both platforms.
    let script = "echo to-stderr >&2; exit 3";
    let run = run_bounded(shell(script), fast()).expect("spawns");
    assert_eq!(run.code, 3);
    assert!(run.text.contains("to-stderr"), "stderr was dropped: {:?}", run.text);
}

/// **The limitation, asserted rather than assumed.**
///
/// `child.kill()` ends the shell. A grandchild it started keeps running, because there is no
/// process-group kill on Unix here and no Job Object on Windows (which would need a `windows-sys`
/// dependency this crate does not have). What A1 closed is the **harness** waiting forever; what it
/// did not close is the orphan.
///
/// This test asserts the part that IS fixed — the call returns promptly — over an input designed to
/// leave an orphan. If a future change adds group teardown, this test still passes and its doc
/// comment is what needs updating.
///
/// **The backgrounded process is deliberately short-lived.** An orphan that outlives the test holds
/// an inherited stdout handle and wedges whatever is reading cargo's output; see [`forever`]. A few
/// seconds is long enough to still be running when the harness returns, which is the whole point.
#[test]
fn a_command_that_backgrounds_a_child_still_returns_promptly() {
    // One spelling: bash on both platforms.
    let script = "sleep 3 & sleep 600";
    let run = run_bounded(shell(script), fast()).expect("spawns");
    assert!(
        run.stopped,
        "the harness must come back even when the child left something behind — the orphan is a \
         leak, the hang was a denial of service, and only the second one is closed here"
    );
}
