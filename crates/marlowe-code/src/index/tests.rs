use std::path::{Path, PathBuf};

use marlowe_os::workspace::LocalFs;

use super::*;

pub(crate) struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("marlowe-code-{tag}-{}-{}", std::process::id(), nanos()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    pub fn put(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

#[test]
fn db_path_is_stable_per_root_and_case_insensitive_on_windows() {
    let a = db_path(Path::new("C:/home"), Path::new(r"C:\Repo\x"));
    assert_eq!(a, db_path(Path::new("C:/home"), Path::new("C:/Repo/x/")));
    assert!(a.starts_with("C:/home/index"));
    if cfg!(windows) {
        assert_eq!(a, db_path(Path::new("C:/home"), Path::new(r"c:\repo\X")));
    }
}

#[test]
fn rel_keeps_the_file_spelling_and_refuses_outside_paths() {
    let t = Tmp::new("rel");
    let idx = Index::in_memory(&t.0).unwrap();
    assert_eq!(idx.rel(&t.0.join("src").join("Main.rs")).as_deref(), Some("src/Main.rs"));
    assert_eq!(idx.rel(&t.0), None);
    assert_eq!(idx.rel(Path::new("/elsewhere/a.rs")), None);
    let sibling = PathBuf::from(format!("{}-other", t.0.display())).join("a.rs");
    assert_eq!(idx.rel(&sibling), None, "a sibling with the root as a string prefix is outside");
}

#[cfg(not(target_arch = "aarch64"))]
#[test]
fn refresh_is_incremental_and_honours_gitignore() {
    let t = Tmp::new("refresh");
    t.put("src/lib.rs", "pub struct Engine;\nimpl Engine { pub fn start(&self) { boot(); } }\nfn boot() {}\n");
    t.put("src/util.py", "def helper():\n    return 1\n");
    t.put("gen/big.rs", "fn generated() {}\n");
    t.put(".gitignore", "gen/\n*.tmp.rs\n");
    t.put("src/x.tmp.rs", "fn ignored() {}\n");
    t.put("target/debug/out.rs", "fn built() {}\n");
    t.put("notes.txt", "fn not_code() {}\n");
    let idx = Index::in_memory(&t.0).unwrap();
    let s = idx.refresh(&LocalFs).unwrap();
    assert_eq!((s.files, s.tagged, s.removed), (2, 2, 0), "{s:?}");
    assert!(idx.defs("generated").is_empty() && idx.defs("ignored").is_empty() && idx.defs("built").is_empty());

    let start = idx.defs("start");
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].container.as_deref(), Some("Engine"));
    assert_eq!(start[0].path, t.0.join("src").join("lib.rs"));
    assert_eq!((start[0].line, start[0].kind.as_str()), (2, "method"));
    assert_eq!(idx.refs("boot", 10).len(), 1);

    // Nothing changed: nothing is re-tagged.
    let s = idx.refresh(&LocalFs).unwrap();
    assert_eq!((s.tagged, s.removed), (0, 0));

    // A changed file is re-tagged; a deleted one is dropped.
    std::thread::sleep(std::time::Duration::from_millis(20));
    t.put("src/util.py", "def helper2():\n    return 2\n");
    std::fs::remove_file(t.0.join("src/lib.rs")).unwrap();
    let s = idx.refresh(&LocalFs).unwrap();
    assert_eq!((s.tagged, s.removed), (1, 1), "{s:?}");
    assert!(idx.defs("helper").is_empty());
    assert_eq!(idx.defs("helper2").len(), 1);
    assert!(idx.defs("Engine").is_empty());
}

#[cfg(not(target_arch = "aarch64"))]
#[test]
fn update_file_retags_one_file_and_drops_a_deleted_one() {
    let t = Tmp::new("update");
    let f = t.put("a.rs", "fn one() {}\n");
    let idx = Index::in_memory(&t.0).unwrap();
    idx.refresh(&LocalFs).unwrap();
    std::fs::write(&f, "fn two() {}\n").unwrap();
    assert!(idx.update_file(&LocalFs, &f).unwrap());
    assert!(idx.defs("one").is_empty());
    assert_eq!(idx.defs("two").len(), 1);
    assert!(!idx.update_file(&LocalFs, &f).unwrap(), "same content: no change");
    std::fs::remove_file(&f).unwrap();
    assert!(idx.update_file(&LocalFs, &f).unwrap());
    assert!(!idx.contains(&f));
}

#[cfg(not(target_arch = "aarch64"))]
#[test]
fn search_ranks_exact_then_prefix_then_substring_and_filters_kind() {
    let t = Tmp::new("search");
    t.put("a.rs", "struct Parse;\nfn parse_all() {}\nfn reparse() {}\nfn parse() {}\n");
    let idx = Index::in_memory(&t.0).unwrap();
    idx.refresh(&LocalFs).unwrap();
    let names: Vec<String> = idx.search("parse", None, 10).into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["parse", "Parse", "parse_all", "reparse"]);
    let names: Vec<String> = idx.search("parse", Some("class"), 10).into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["Parse"]);
    assert_eq!(idx.search("_", None, 10).len(), 1, "LIKE wildcards are literal");
}

#[cfg(not(target_arch = "aarch64"))]
#[test]
fn an_on_disk_index_survives_reopening() {
    let t = Tmp::new("disk");
    t.put("src/a.rs", "fn kept() {}\n");
    let home = Tmp::new("disk-home");
    let db = db_path(&home.0, &t.0);
    {
        let idx = Index::open(&db, &t.0).unwrap();
        assert_eq!(idx.refresh(&LocalFs).unwrap().tagged, 1);
    }
    let idx = Index::open(&db, &t.0).unwrap();
    assert_eq!(idx.defs("kept").len(), 1);
    assert_eq!(idx.refresh(&LocalFs).unwrap().tagged, 0, "unchanged files are not re-tagged after reopening");
}
