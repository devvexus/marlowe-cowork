use std::path::{Path, PathBuf};

use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-items-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

#[test]
fn folders_skip_vcs_build_output_and_ignored_dirs() {
    let ws = temp("folders");
    for d in [".git/objects", "target/debug", "node_modules/x", "secret_build/out", "ml", "notebooks", "services"] {
        put(&ws, &format!("{d}/f.txt"), "x");
    }
    put(&ws, ".gitignore", "secret_build/\n");
    put(&ws, "README.md", "hi");
    let got = resolve("folders", &ws).unwrap();
    assert_eq!(got.items, ["ml", "notebooks", "services"], "{got:?}");
    assert!(got.note.is_none());
}

#[test]
fn fewer_than_two_folders_adds_the_top_level_files() {
    let ws = temp("folders1");
    put(&ws, "src/lib.rs", "x");
    put(&ws, "README.md", "x");
    put(&ws, "Cargo.toml", "x");
    let got = resolve("directories", &ws).unwrap();
    assert_eq!(got.items.len(), 2, "{got:?}");
    assert_eq!(got.items[0], "src");
    assert!(got.items[1].starts_with("the top-level files (") && got.items[1].contains("README.md"), "{got:?}");
    // Two folders: no extra item.
    put(&ws, "docs/a.md", "x");
    assert_eq!(resolve("folders", &ws).unwrap().items, ["docs", "src"]);
}

#[test]
fn an_empty_workspace_says_what_is_there() {
    let ws = temp("empty");
    let e = resolve("folders", &ws).unwrap_err();
    assert!(e.contains("no folders or files") && e.contains("is empty"), "{e}");
    put(&ws, "a.txt", "x");
    put(&ws, "b.txt", "x");
    let e = resolve("*.rs", &ws).unwrap_err();
    assert!(e.contains("No files match") && e.contains("a.txt, b.txt"), "{e}");
    let e = resolve("the interesting ones", &ws).unwrap_err();
    assert!(e.contains("folders | packages | commits:N"), "{e}");
}

#[test]
fn packages_from_a_cargo_workspace() {
    let ws = temp("cargo");
    put(&ws, "Cargo.toml", "[workspace]\nresolver = \"2\"\nmembers = [\n  \"crates/*\", # all of them\n  \"tools/cli\",\n]\nexclude = [\"crates/old\"]\n\n[workspace.dependencies]\nmembers = [\"nope\"]\n");
    for c in ["crates/alpha", "crates/beta", "crates/old", "tools/cli", "tools/other"] {
        put(&ws, &format!("{c}/Cargo.toml"), "[package]\nname = \"x\"\n");
    }
    let got = resolve("crates", &ws).unwrap();
    assert_eq!(got.items, ["crates/alpha", "crates/beta", "tools/cli"], "{got:?}");
}

#[test]
fn packages_from_mixed_manifests_within_two_folders() {
    let ws = temp("mixed");
    put(&ws, "web/package.json", "{}");
    put(&ws, "services/api/pyproject.toml", "");
    put(&ws, "services/worker/go.mod", "");
    put(&ws, "deep/a/b/Cargo.toml", "");
    put(&ws, "node_modules/lib/package.json", "{}");
    put(&ws, "notes/readme.md", "");
    let got = resolve("packages", &ws).unwrap();
    assert_eq!(got.items, ["services/api", "services/worker", "web"], "{got:?}");
}

#[test]
fn packages_fall_back_to_folders_with_a_note() {
    let ws = temp("nopkg");
    put(&ws, "a/x.txt", "");
    put(&ws, "b/y.txt", "");
    let got = resolve("modules", &ws).unwrap();
    assert_eq!(got.items, ["a", "b"]);
    assert!(got.note.unwrap().contains("so these are the folders"));
}

#[test]
fn commits_come_from_git_log() {
    let ws = temp("commits");
    git(&ws, &["init", "-q"]);
    for (i, msg) in ["first", "second", "third", "fourth"].iter().enumerate() {
        put(&ws, "f.txt", &i.to_string());
        git(&ws, &["add", "."]);
        git(&ws, &["commit", "-q", "-m", msg]);
    }
    let got = resolve("commits:3", &ws).unwrap();
    assert_eq!(got.items.len(), 3);
    assert!(got.items[0].ends_with(" fourth") && got.items[2].ends_with(" second"), "{got:?}");
    assert_eq!(resolve("commits", &ws).unwrap().items.len(), 4, "default 5, only 4 exist");
    assert_eq!(resolve("last 2 commits", &ws).unwrap().items.len(), 2);
    let many = resolve("commits:99", &ws).unwrap();
    assert!(many.note.unwrap().contains("the most is 20"));
    // Not a repository.
    let plain = temp("nogit");
    put(&plain, "a.txt", "");
    let e = resolve("commits:3", &plain).unwrap_err();
    assert!(e.contains("git log") && e.contains("a.txt"), "{e}");
}

#[test]
fn a_glob_lists_files_and_is_capped_at_24() {
    let ws = temp("glob");
    for i in 0..30 {
        put(&ws, &format!("src/m{i:02}.rs"), "");
    }
    put(&ws, "src/readme.md", "");
    put(&ws, "target/x.rs", "");
    put(&ws, "docs/a.rs", "");
    let few = resolve("docs/*.rs", &ws).unwrap();
    assert_eq!(few.items, ["docs/a.rs"]);
    let e = resolve("files:**/*.rs", &ws).unwrap_err();
    assert!(e.contains("32 items") || e.contains("31 items"), "{e}");
    assert!(e.contains("at most 24") && e.contains("Narrow it"), "{e}");
    let some = resolve("src/m0*.rs", &ws).unwrap();
    assert_eq!(some.items.len(), 10, "{some:?}");
    assert!(!resolve("src/*.md", &ws).unwrap().items.iter().any(|f| f.starts_with("target")));
}

#[test]
fn aliases_and_spelling_are_read() {
    let ws = temp("alias");
    put(&ws, "a/Cargo.toml", "");
    put(&ws, "b/Cargo.toml", "");
    for s in ["Folders", "dirs", "top-level folders", " `folders` "] {
        assert_eq!(resolve(s, &ws).unwrap().items, ["a", "b"], "{s}");
    }
    for s in ["crates", "Packages", "projects", "modules"] {
        assert_eq!(resolve(s, &ws).unwrap().items, ["a", "b"], "{s}");
    }
}

#[test]
fn wild_matches_stars() {
    assert!(wild("*", "x") && wild("a*", "abc") && wild("*c", "abc") && wild("a*c", "abc") && wild("a*b*c", "aXbYc"));
    assert!(!wild("a*c", "ab") && !wild("a*c", "ac!") && !wild("abc", "abd"));
}

#[test]
fn what_models_send_instead_is_read_not_refused() {
    let ws = temp("lenient");
    put(&ws, "services/api/main.py", "");
    put(&ws, "services/worker/main.py", "");
    put(&ws, "ml/train.py", "");
    // The workspace path itself, absolute or `.`: its folders.
    assert_eq!(resolve(&ws.display().to_string(), &ws).unwrap().items, ["ml", "services"]);
    assert_eq!(resolve(".", &ws).unwrap().items, ["ml", "services"]);
    // A folder inside it: its subfolders, named from the workspace.
    assert_eq!(resolve("services", &ws).unwrap().items, ["services/api", "services/worker"]);
    // A folder outside the workspace is not a source.
    let other = temp("lenient-other");
    assert!(resolve(&other.display().to_string(), &ws).is_err());
    // A JSON list is the list.
    let got = resolve(r#"["alpha", "beta"]"#, &ws).unwrap();
    assert_eq!(got.items, ["alpha", "beta"]);
    assert!(got.note.unwrap().contains("read as `items`"));
    assert!(resolve("[1, 2]", &ws).is_err(), "a list of numbers is nothing");
    // The cap holds for a list too.
    let long = serde_json::to_string(&(0..30).map(|i| format!("i{i}")).collect::<Vec<_>>()).unwrap();
    assert!(resolve(&long, &ws).unwrap_err().contains("at most 24"));
    // A git command line is commits.
    let repo = temp("lenient-git");
    git(&repo, &["init", "-q"]);
    put(&repo, "a.txt", "1");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    assert_eq!(resolve("git log -3 --format=%H", &repo).unwrap().items.len(), 1);
    assert_eq!(resolve("git -c core.pager=cat log --oneline", &repo).unwrap().items.len(), 1);
}

#[test]
fn a_glob_that_names_only_folders_is_those_folders() {
    let ws = temp("globdirs");
    put(&ws, "crates/cli/src/lib.rs", "");
    put(&ws, "crates/ingest/src/lib.rs", "");
    for sel in ["crates/*", "crates/*/"] {
        let found = resolve(sel, &ws).unwrap();
        assert_eq!(found.items, ["crates/cli", "crates/ingest"], "{sel}");
        assert!(found.note.is_some());
    }
    assert_eq!(resolve("crates/**/lib.rs", &ws).unwrap().items.len(), 2, "files still win when the glob matches files");
}

#[test]
fn free_text_in_items_from_is_read_as_a_list_when_it_is_one() {
    let ws = temp("aslist");
    put(&ws, "services/api/main.py", "");
    put(&ws, "services/worker/worker.py", "");
    assert_eq!(as_list("min-p, top-k, nucleus", &ws).unwrap().items, ["min-p", "top-k", "nucleus"]);
    assert_eq!(as_list("services/api/main.py services/worker/worker.py", &ws).unwrap().items.len(), 2);
    assert!(as_list("min-p top-k nucleus", &ws).is_none(), "words that are not paths are not a list");
    assert!(as_list("folders", &ws).is_none());
}
