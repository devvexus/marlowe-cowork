use super::*;

fn notice(session: &str, id: &str, kind: NoticeKind) -> BgNotice {
    BgNotice { session: session.into(), id: id.into(), kind, text: format!("{id} {kind:?}") }
}

#[test]
fn notices_are_taken_per_session_in_order() {
    let n = Notices::default();
    n.push(notice("a", "j1", NoticeKind::Lines));
    n.push(notice("b", "j2", NoticeKind::Exit));
    n.push(notice("a", "j1", NoticeKind::Exit));
    let mine = n.take("a");
    assert_eq!(mine.iter().map(|m| m.kind).collect::<Vec<_>>(), [NoticeKind::Lines, NoticeKind::Exit]);
    assert!(n.take("a").is_empty());
    assert_eq!(n.take("b").len(), 1);
}

#[test]
fn only_exits_and_matches_wake_and_a_killed_job_is_forgotten() {
    let n = Notices::default();
    n.push(notice("lines-only", "j1", NoticeKind::Lines));
    n.push(notice("user", "j2", NoticeKind::UserShell));
    assert!(n.waking().is_empty());
    n.push(notice("exited", "j3", NoticeKind::Exit));
    n.push(notice("matched", "j4", NoticeKind::Match));
    n.push(notice("exited", "j5", NoticeKind::Exit));
    assert_eq!(n.waking(), ["exited", "matched"]);
    n.forget("j3");
    n.forget("j5");
    assert_eq!(n.waking(), ["matched"]);
    // The model has heard of j3: the exit notice that races in afterwards is not queued.
    n.push(notice("exited", "j3", NoticeKind::Exit));
    assert_eq!(n.waking(), ["matched"]);
}

#[test]
fn durations_read_naturally() {
    assert_eq!(fmt_secs(Duration::from_millis(1_500)), "1.5s");
    assert_eq!(fmt_secs(Duration::from_secs(42)), "42s");
    assert_eq!(fmt_secs(Duration::from_secs(185)), "3m05s");
}

#[test]
fn only_old_log_files_are_pruned() {
    let dir = std::env::temp_dir().join(format!("marlowe-jobs-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("j1-1.log");
    let fresh = dir.join("j2-2.log");
    let other = dir.join("keep.txt");
    for p in [&old, &fresh, &other] {
        std::fs::write(p, "x").unwrap();
    }
    let ancient = std::time::SystemTime::now() - Duration::from_secs((LOG_DAYS + 1) * 24 * 3600);
    for p in [&old, &other] {
        std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(ancient).unwrap();
    }
    prune_logs(&dir);
    assert!(!old.exists() && fresh.exists() && other.exists(), "only week-old .log files go");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Starts `script` in bash as a background job with a log `cap`.
fn start(jobs: &HostJobs, tools: &crate::BuiltinTools, script: &str, cap: u64) -> Arc<Job> {
    let cmd = crate::shell::bash_command(script).unwrap();
    jobs.launch(
        tools.workspace_port(),
        Launch {
            req: ExecRequest::from_command(&cmd),
            display: script.to_string(),
            session: "unit".into(),
            call_id: None,
            foreground: false,
            cap,
            user: false,
            hidden: false,
            leftovers: Leftovers::Release,
            timeout: None,
        },
    )
    .unwrap()
}

fn unit_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-jobs-unit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_exit_notice_reads_the_end_of_a_long_log_where_failures_print() {
    let dir = unit_dir("tail");
    let jobs = HostJobs::new(dir.clone());
    let tools = crate::BuiltinTools::new(None);
    let job = start(
        &jobs,
        &tools,
        "for i in $(seq 1 3000); do echo \"warming up step $i of 3000\"; done; echo 'FAILED: loss is nan at step 2999'; exit 3",
        BG_LOG_CAP,
    );
    assert_eq!(job.wait_exit(Duration::from_secs(60)), Some(3));
    let (_, text) = job.exit_text_in(3, 4_000);
    assert!(text.contains("FAILED: loss is nan at step 2999"), "the failure at the end is in the notice: {text}");
    assert!(text.contains("of the log are not summarised here"), "{text}");
    assert!(text.contains(", 3001 lines]"), "the pointer counts the whole log: {text}");
    let (tail, skipped) = job.read_tail(4_000);
    assert!(skipped > 0 && tail.len() <= 4_000 && tail.starts_with(b"warming up"), "the tail starts at a line");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_background_job_that_passes_its_cap_is_stopped_and_the_notice_says_so() {
    let dir = unit_dir("cap");
    let jobs = HostJobs::new(dir.clone());
    let tools = crate::BuiltinTools::new(None);
    // Writes far more than the 4 KiB cap, then would run for a minute.
    let job = start(&jobs, &tools, "yes 'chatter chatter chatter' | head -n 5000; sleep 60", 4 << 10);
    let code = job.wait_exit(Duration::from_secs(30));
    assert!(code.is_some(), "the monitor stopped it at the cap");
    let (_, text) = job.exit_text(code.unwrap());
    assert!(text.contains("was stopped") && text.contains("its output passed the 4 KiB cap"), "{text}");
    assert!(job.len() <= 4 << 10, "the log stops at the cap");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_host_nobody_set_up_keeps_its_logs_in_the_sandbox_it_is_pinned_to() {
    let sandbox = unit_dir("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    let sandbox = sandbox.canonicalize().unwrap();
    let tools = crate::BuiltinTools::new_sandbox(sandbox.clone()).with_shell_dir(sandbox.clone());
    let jobs = Arc::new(HostJobs::default());
    jobs.bind(&tools);
    let dir = lock(&jobs.dir).clone();
    assert_eq!(dir, sandbox.join(SANDBOX_LOGS));
    assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n", "git ignores the logs");
    assert!(tools.trusted_read(&dir.join("j1.log").to_string_lossy()));
    // A host with no sandbox keeps the temp folder, and a configured one its own.
    let plain = Arc::new(HostJobs::default());
    plain.bind(&crate::BuiltinTools::new(None));
    assert_eq!(*lock(&plain.dir), std::env::temp_dir().join("marlowe-jobs"));
    let _ = std::fs::remove_dir_all(sandbox);
}

#[test]
fn a_host_without_the_middleware_says_it_cannot_deliver_notices() {
    let jobs = HostJobs::default();
    assert!(!jobs.delivers_notices());
    jobs.note_delivering();
    assert!(jobs.delivers_notices());
}
