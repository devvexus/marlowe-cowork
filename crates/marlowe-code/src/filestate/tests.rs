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
    std::fs::write(&a, "x\nadded\n").unwrap();
    std::fs::remove_file(&b).unwrap();
    let changes = s.poll(&LocalFs);
    assert_eq!(changes.len(), 2, "{changes:?}");
    let text = reminder(&changes, &|p| p.file_name().unwrap().to_string_lossy().into_owned());
    assert!(text.contains("a.txt: modified.\n@@ line 2 @@\n+added"), "{text}");
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
