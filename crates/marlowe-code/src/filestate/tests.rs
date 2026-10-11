use std::path::PathBuf;

use marlowe_os::workspace::{LocalFs, Workspace};

use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-code-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn see(states: &mut FileStates, p: &Path) {
    let bytes = LocalFs.read(p).unwrap();
    let meta = LocalFs.metadata(p).unwrap();
    states.record(p, &bytes, Some(&meta));
}

#[test]
fn unread_fresh_stale_and_absent() {
    let d = temp("check");
    let p = d.join("a.txt");
    let mut s = FileStates::new();
    assert_eq!(s.check(&LocalFs, &p), Freshness::Absent);
    std::fs::write(&p, "one\ntwo\n").unwrap();
    assert_eq!(s.check(&LocalFs, &p), Freshness::Unread);
    see(&mut s, &p);
    assert_eq!(s.check(&LocalFs, &p), Freshness::Fresh);
    std::fs::write(&p, "one\nTWO\n").unwrap();
    match s.check(&LocalFs, &p) {
        Freshness::Stale { diff } => assert_eq!(diff, "@@ line 2 @@\n-two\n+TWO"),
        other => panic!("{other:?}"),
    }
    // Same bytes rewritten (new mtime): still fresh.
    see(&mut s, &p);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&p, "one\nTWO\n").unwrap();
    assert_eq!(s.check(&LocalFs, &p), Freshness::Fresh);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn poll_reports_changes_once_and_deletions() {
    let d = temp("poll");
    let (a, b) = (d.join("a.txt"), d.join("b.txt"));
    std::fs::write(&a, "x\n").unwrap();
    std::fs::write(&b, "y\n").unwrap();
    let mut s = FileStates::new();
    see(&mut s, &a);
    see(&mut s, &b);
    assert!(s.poll(&LocalFs).is_empty());
    std::fs::write(&a, "X\nadded\n").unwrap();
    std::fs::remove_file(&b).unwrap();
    let changes = s.poll(&LocalFs);
    assert_eq!(changes.len(), 2, "{changes:?}");
    let text = reminder(&changes, &|p| p.file_name().unwrap().to_string_lossy().into_owned());
    assert!(text.contains("a.txt: modified.\n@@ line 1 @@\n-x\n+X\n+added"), "{text}");
    assert!(text.contains("b.txt: deleted."), "{text}");
    assert!(s.poll(&LocalFs).is_empty(), "a change is reported once");
    assert_eq!(s.check(&LocalFs, &a), Freshness::Fresh, "the reminder counts as seeing it");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_table_is_bounded() {
    let mut s = FileStates::new();
    for i in 0..MAX_TRACKED + 10 {
        s.record(Path::new(&format!("C:\\x\\{i}.txt")), b"x", None);
    }
    assert_eq!(s.len(), MAX_TRACKED);
    assert!(s.get(Path::new("C:\\x\\0.txt")).is_none(), "oldest dropped");
    assert!(s.get(Path::new(&format!("C:\\x\\{}.txt", MAX_TRACKED + 9))).is_some());
}

/// [`LocalFs`] counting reads, to show what a poll reads.
#[derive(Default)]
struct Counting(std::sync::atomic::AtomicUsize);

impl Workspace for Counting {
    fn kind(&self) -> String {
        "local".into()
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LocalFs.read(path)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        LocalFs.write(path, bytes)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        LocalFs.rename(from, to)
    }
    fn walk(&self, dir: &Path, opts: &marlowe_os::workspace::WalkOpts) -> std::io::Result<Vec<marlowe_os::workspace::WalkEntry>> {
        LocalFs.walk(dir, opts)
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Meta> {
        LocalFs.metadata(path)
    }
    fn exec(
        &self,
        req: &marlowe_os::workspace::ExecRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> std::io::Result<marlowe_os::workspace::ExecOutput> {
        LocalFs.exec(req, cancelled)
    }
}

fn lines(n: usize, tag: &str) -> String {
    (0..n).map(|i| format!("{tag} line {i}\n")).collect()
}

/// A change the reminder does not show whole (no snapshot above 256 KB, a diff cut at 40 lines)
/// leaves the agent's view as it was: an edit stays refused until the agent reads again, and the
/// change is not reported again until the file changes again.
#[test]
fn a_change_not_shown_whole_is_not_seen() {
    let d = temp("unshown");
    let show = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
    // Above the snapshot size: sizes only.
    let big = d.join("big.json");
    std::fs::write(&big, lines(30_000, "a")).unwrap();
    assert!(std::fs::metadata(&big).unwrap().len() as usize > SNAPSHOT_MAX_BYTES);
    let mut s = FileStates::new();
    see(&mut s, &big);
    std::fs::write(&big, lines(30_001, "b")).unwrap();
    let changes = s.poll(&LocalFs);
    assert!(matches!(changes[..], [Change { what: Changed::Unshown { .. }, .. }]), "{changes:?}");
    let text = reminder(&changes, &show);
    assert!(text.contains("big.json: modified (") && text.contains("too large to diff), read it again before editing it."), "{text}");
    assert!(matches!(s.check(&LocalFs, &big), Freshness::Stale { .. }), "not seen: an edit is refused");
    assert!(s.poll(&LocalFs).is_empty(), "reported once");
    s.begin_turn();
    assert!(s.poll(&LocalFs).is_empty(), "the same change is not reported again next turn");
    std::fs::write(&big, lines(30_002, "c")).unwrap();
    assert_eq!(s.poll(&LocalFs).len(), 1, "a further change in a later turn is");
    see(&mut s, &big);
    assert_eq!(s.check(&LocalFs, &big), Freshness::Fresh);

    // A diff over 40 changed lines is cut: shown in part, not seen.
    let cfg = d.join("cfg.yaml");
    std::fs::write(&cfg, lines(100, "a")).unwrap();
    see(&mut s, &cfg);
    std::fs::write(&cfg, lines(100, "b")).unwrap();
    let changes = s.poll(&LocalFs);
    let text = reminder(&changes, &show);
    assert!(text.contains("cfg.yaml: modified; only part of the change is shown, read it again before editing it."), "{text}");
    assert!(text.contains("more changed lines"), "{text}");
    assert!(matches!(s.check(&LocalFs, &cfg), Freshness::Stale { .. }));
    let _ = std::fs::remove_dir_all(&d);
}

/// A log the agent read and something keeps appending to: a one-line note at most once per turn,
/// not the new lines every step; the agent's view stays what it read.
#[test]
fn an_appended_log_is_noted_once_per_turn() {
    let d = temp("append");
    let log = d.join("train.log");
    std::fs::write(&log, lines(3, "step")).unwrap();
    let mut s = FileStates::new();
    see(&mut s, &log);
    let append = |n: usize| {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(lines(n, "more").as_bytes()).unwrap();
    };
    append(5);
    let changes = s.poll(&LocalFs);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].what, Changed::Appended { lines: 5, bytes: lines(5, "more").len() as u64 });
    let text = reminder(&changes, &|p| p.file_name().unwrap().to_string_lossy().into_owned());
    assert!(
        text.contains("train.log: 5 lines (") && text.contains("appended, earlier content unchanged; not shown, read it again before editing it."),
        "{text}"
    );
    for _ in 0..5 {
        append(7);
        assert!(s.poll(&LocalFs).is_empty(), "at most once per turn");
    }
    assert!(matches!(s.check(&LocalFs, &log), Freshness::Stale { .. }), "the appended lines were not shown");
    s.begin_turn();
    let changes = s.poll(&LocalFs);
    assert!(matches!(changes[..], [Change { what: Changed::Appended { lines: 40, .. }, .. }]), "{changes:?}");
    // Reading it again resets the view, and the next append is news in the same turn.
    see(&mut s, &log);
    append(1);
    assert!(matches!(s.poll(&LocalFs)[..], [Change { what: Changed::Appended { lines: 1, .. }, .. }]));
    let _ = std::fs::remove_dir_all(&d);
}

/// Above the poll's read ceiling a changed file is judged by size and mtime, never read.
#[test]
fn a_poll_does_not_read_files_above_its_ceiling() {
    let d = temp("ceiling");
    let p = d.join("data.csv");
    std::fs::write(&p, "x,y\n".repeat(POLL_READ_MAX_BYTES as usize / 4 + 10)).unwrap();
    let mut s = FileStates::new();
    see(&mut s, &p);
    std::fs::write(&p, "x,z\n".repeat(POLL_READ_MAX_BYTES as usize / 4 + 11)).unwrap();
    let ws = Counting::default();
    let changes = s.poll(&ws);
    assert!(matches!(changes[..], [Change { what: Changed::Unshown { .. }, .. }]), "{changes:?}");
    assert_eq!(ws.0.load(std::sync::atomic::Ordering::SeqCst), 0, "stat only");
    assert!(matches!(s.check(&LocalFs, &p), Freshness::Stale { .. }));
    let _ = std::fs::remove_dir_all(&d);
}
