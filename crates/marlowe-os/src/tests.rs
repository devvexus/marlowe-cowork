//! Job Object behaviour: a grandchild started through `spawn_in_job` dies with the job, and
//! `terminate` / `kill_tree` take the whole tree down.

#[test]
fn os_errors_name_the_failing_call() {
    let e = crate::OsError::Win32 { call: "AssignProcessToJobObject", code: 5 };
    assert_eq!(e.to_string(), "AssignProcessToJobObject failed (os error 5)");
}

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use crate::{spawn_in_job, spawn_in_job_with_flags, JobObject};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A fresh marker path in the temp dir (no spaces in the file name).
    fn marker(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "marlowe-os-{tag}-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// `cmd /c` that starts a detached grandchild which writes `marker` after ~2 s, then runs
    /// `tail` (a cmd fragment) itself.
    fn tree(marker: &Path, tail: &str) -> Command {
        let mut c = Command::new("cmd.exe");
        c.raw_arg(format!(
            "/d /c start \"\" /b cmd /d /c \"ping -n 3 127.0.0.1 >nul & echo up> {}\"{tail}",
            marker.display()
        ));
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c
    }

    fn wait_for(mut done: impl FnMut() -> bool, limit: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    #[test]
    fn a_job_child_runs_and_its_output_is_read() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/d", "/c", "echo hello"]).stdout(Stdio::piped());
        let mut jc = spawn_in_job_with_flags(&mut cmd, CREATE_NO_WINDOW).unwrap();
        assert!(jc.id() > 0);
        let mut out = jc.child_mut().stdout.take().unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut out, &mut text).unwrap();
        assert!(jc.child_mut().wait().unwrap().success());
        assert_eq!(text.trim(), "hello");
    }

    #[test]
    fn a_grandchild_dies_when_the_job_is_dropped() {
        let m = marker("drop");
        let mut jc = spawn_in_job(&mut tree(&m, "")).unwrap();
        // The direct child exits at once; the grandchild is still in the job.
        assert!(jc.child_mut().wait().unwrap().success());
        assert!(jc.job().active_processes().unwrap() >= 1, "the grandchild is in the job");
        let (_child, job) = jc.into_parts();
        drop(job);
        std::thread::sleep(Duration::from_millis(3_500));
        assert!(!m.exists(), "the grandchild outlived the job");
    }

    #[test]
    fn released_jobs_let_background_processes_finish() {
        let m = marker("release");
        let mut jc = spawn_in_job(&mut tree(&m, "")).unwrap();
        assert!(jc.child_mut().wait().unwrap().success());
        let (_child, job) = jc.into_parts();
        job.release().unwrap();
        assert!(wait_for(|| m.exists(), Duration::from_secs(15)), "the grandchild was killed");
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn kill_tree_takes_the_child_and_its_grandchildren() {
        let m = marker("kill");
        let mut jc = spawn_in_job(&mut tree(&m, " & ping -n 30 127.0.0.1 >nul")).unwrap();
        assert!(wait_for(|| jc.job().active_processes().unwrap() >= 2, Duration::from_secs(10)));
        let started = Instant::now();
        jc.kill_tree().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(jc.child_mut().try_wait().unwrap().is_some(), "the child was reaped");
        assert!(wait_for(|| jc.job().active_processes().unwrap() == 0, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(3_000));
        assert!(!m.exists(), "a grandchild survived kill_tree");
    }

    #[test]
    fn terminate_empties_the_job_and_the_job_stays_usable() {
        let job = JobObject::new_kill_on_close().unwrap();
        let mut a = Command::new("cmd.exe");
        a.args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]).stdout(Stdio::null());
        a.creation_flags(CREATE_NO_WINDOW);
        let mut child = a.spawn().unwrap();
        job.assign(&child).unwrap();
        assert!(job.active_processes().unwrap() >= 1);
        job.terminate(7).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        assert!(wait_for(|| job.active_processes().unwrap() == 0, Duration::from_secs(5)));
        // Still usable: a new process can join and is counted.
        let mut b = Command::new("cmd.exe");
        b.args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]).stdout(Stdio::null());
        b.creation_flags(CREATE_NO_WINDOW);
        let mut child = b.spawn().unwrap();
        job.assign(&child).unwrap();
        assert!(job.active_processes().unwrap() >= 1);
        drop(job);
        assert!(child.wait().is_ok());
    }

    #[test]
    fn a_missing_program_is_an_io_error_not_a_leak() {
        let mut cmd = Command::new("definitely-not-a-program-marlowe.exe");
        assert!(matches!(spawn_in_job(&mut cmd), Err(crate::OsError::Io(_))));
    }
}
