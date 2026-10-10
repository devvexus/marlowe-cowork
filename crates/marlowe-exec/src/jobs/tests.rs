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
