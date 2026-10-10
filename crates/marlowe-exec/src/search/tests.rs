//! The rg fixture (A9): each case names the ripgrep command it reproduces, builds a tree, runs the
//! tool and compares the whole result with what that command prints (expected text written from
//! rg's documented output: `path:line:text`, `path-line-text` context, `--` between groups,
//! `--sort path`). ripgrep itself is not installed on CI machines, so the expectations are
//! literal.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use marlowe_os::workspace::LocalFs;
use serde_json::{json, Value};

use super::{glob, grep, SearchEnv};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Tree {
    root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn tree(files: &[(&str, &[u8])]) -> Tree {
    let root = std::env::temp_dir().join(format!(
        "marlowe-search-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = SystemTime::now() - Duration::from_secs(36_000);
    for (rel, bytes) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        set_mtime(&p, base);
    }
    Tree { root }
}

fn set_mtime(p: &std::path::Path, at: SystemTime) {
    std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(at).unwrap();
}

impl Tree {
    /// Makes `rel` this many seconds newer than the rest.
    fn newer(&self, rel: &str, secs: u64) {
        set_mtime(&self.root.join(rel), SystemTime::now() - Duration::from_secs(36_000) + Duration::from_secs(secs));
    }
}

/// The shared tree: a git repo with ignored files, hidden files, nested sources and CRLF text.
fn base() -> Tree {
    tree(&[
        (".git/config", b"[core]\nhello = 1\n"),
        (".gitignore", b"ignored/\n*.log\n"),
        (".github/ci.yml", b"name: ci\nrun: cargo test\n"),
        ("README.md", b"# Demo\nhello docs\n"),
        ("build.log", b"hello log\n"),
        ("ignored/secret.txt", b"hello secret\n"),
        ("src/lib.rs", b"pub fn helper() {}\npub fn Helper2() {}\n// TODO: refactor\n// todo: lowercase\n"),
        ("src/main.rs", b"fn main() {\n    println!(\"hello world\");\n    helper();\n}\n"),
        ("src/util/mod.rs", b"pub mod strings;\n"),
        ("src/util/strings.rs", b"pub fn shout(s: &str) -> String {\n    s.to_uppercase()\n}\n"),
        ("docs/guide.md", b"hello from the guide\nsecond line\n"),
        ("crlf.txt", b"alpha\r\nbeta\r\n"),
    ])
}

fn env(t: &Tree) -> SearchEnv<'_> {
    SearchEnv { root: Some(&t.root), port: &LocalFs }
}

enum Expect {
    Exact(&'static str),
    Error(&'static str),
}

struct Case {
    /// The ripgrep command this reproduces (with `--hidden --glob '!.git' --sort path`).
    rg: &'static str,
    args: Value,
    expect: Expect,
}

fn check(t: &Tree, tool: fn(&SearchEnv, &Value) -> marlowe_agent::ToolOutcome, cases: Vec<Case>) -> usize {
    let mut failures = Vec::new();
    let n = cases.len();
    for c in cases {
        let out = tool(&env(t), &c.args);
        let ok = match &c.expect {
            Expect::Exact(e) => !out.is_error && out.content == *e,
            Expect::Error(e) => out.is_error && out.content.contains(e),
        };
        if !ok {
            let want = match &c.expect {
                Expect::Exact(e) | Expect::Error(e) => *e,
            };
            failures.push(format!(
                "rg {}\n  args: {}\n  want: {want:?}\n  got:  {:?} (error: {})",
                c.rg, c.args, out.content, out.is_error
            ));
        }
    }
    assert!(failures.is_empty(), "{} of {n} cases differ from ripgrep:\n{}", failures.len(), failures.join("\n"));
    n
}

fn exact(rg: &'static str, args: Value, expect: &'static str) -> Case {
    Case { rg, args, expect: Expect::Exact(expect) }
}

fn err(rg: &'static str, args: Value, expect: &'static str) -> Case {
    Case { rg, args, expect: Expect::Error(expect) }
}

#[test]
fn parity_t11_grep_matches_the_rg_fixture() {
    let t = base();
    // Newest first for files_with_matches.
    t.newer("docs/guide.md", 3_000);
    t.newer("README.md", 2_000);
    t.newer("src/main.rs", 1_000);
    let c = |pattern: &str, rest: Value| {
        let mut v = json!({"pattern": pattern, "output_mode": "content"});
        for (k, x) in rest.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let hello_all = "README.md:2:hello docs\ndocs/guide.md:1:hello from the guide\nsrc/main.rs:2:    println!(\"hello world\");";
    let cases = vec![
        exact("-n hello", c("hello", json!({})), hello_all),
        exact("-l hello (newest first)", json!({"pattern": "hello"}), "docs/guide.md\nREADME.md\nsrc/main.rs"),
        exact("-c hello", json!({"pattern": "hello", "output_mode": "count"}), "README.md:1\ndocs/guide.md:1\nsrc/main.rs:1"),
        exact("-n todo (case-sensitive)", c("todo", json!({})), "src/lib.rs:4:// todo: lowercase"),
        exact(
            "-n -i todo",
            c("todo", json!({"-i": true})),
            "src/lib.rs:3:// TODO: refactor\nsrc/lib.rs:4:// todo: lowercase",
        ),
        exact(
            "-n '(?i)todo'",
            c("(?i)todo", json!({})),
            "src/lib.rs:3:// TODO: refactor\nsrc/lib.rs:4:// todo: lowercase",
        ),
        exact(
            "-N hello",
            c("hello", json!({"-n": false})),
            "README.md:hello docs\ndocs/guide.md:hello from the guide\nsrc/main.rs:    println!(\"hello world\");",
        ),
        exact(
            "-n -C 1 'helper\\(\\)' src",
            c("helper\\(\\)", json!({"-C": 1, "path": "src"})),
            "src/lib.rs:1:pub fn helper() {}\nsrc/lib.rs-2-pub fn Helper2() {}\n--\nsrc/main.rs-2-    println!(\"hello world\");\nsrc/main.rs:3:    helper();\nsrc/main.rs-4-}",
        ),
        exact(
            "-n -A 1 'fn main' src/main.rs",
            c("fn main", json!({"-A": 1, "path": "src/main.rs"})),
            "src/main.rs:1:fn main() {\nsrc/main.rs-2-    println!(\"hello world\");",
        ),
        exact(
            "-n -B 1 'helper\\(\\);' src/main.rs",
            c("helper\\(\\);", json!({"-B": 1, "path": "src/main.rs"})),
            "src/main.rs-2-    println!(\"hello world\");\nsrc/main.rs:3:    helper();",
        ),
        exact(
            "-n -C 2 -A 0 'helper\\(\\);' src/main.rs",
            c("helper\\(\\);", json!({"-C": 2, "-A": 0, "path": "src/main.rs"})),
            "src/main.rs-1-fn main() {\nsrc/main.rs-2-    println!(\"hello world\");\nsrc/main.rs:3:    helper();",
        ),
        exact(
            "-n -A 1 '^pub fn' src/lib.rs (adjacent groups merge)",
            c("^pub fn", json!({"-A": 1, "path": "src/lib.rs"})),
            "src/lib.rs:1:pub fn helper() {}\nsrc/lib.rs:2:pub fn Helper2() {}\nsrc/lib.rs-3-// TODO: refactor",
        ),
        exact(
            "-n -g '*.md' hello",
            c("hello", json!({"glob": "*.md"})),
            "README.md:2:hello docs\ndocs/guide.md:1:hello from the guide",
        ),
        exact("-n -g '*.{md,rs}' hello", c("hello", json!({"glob": "*.{md,rs}"})), hello_all),
        exact("-n -g '*.md *.rs' hello (space-separated)", c("hello", json!({"glob": "*.md *.rs"})), hello_all),
        exact("-n -g 'docs/*.md' hello", c("hello", json!({"glob": "docs/*.md"})), "docs/guide.md:1:hello from the guide"),
        exact("-n -g '!*.md' hello", c("hello", json!({"glob": "!*.md"})), "src/main.rs:2:    println!(\"hello world\");"),
        exact(
            "-c -t rust fn",
            json!({"pattern": "fn", "type": "rust", "output_mode": "count"}),
            "src/lib.rs:2\nsrc/main.rs:1\nsrc/util/strings.rs:1",
        ),
        exact(
            "-c -t md hello",
            json!({"pattern": "hello", "type": "md", "output_mode": "count"}),
            "README.md:1\ndocs/guide.md:1",
        ),
        err("-t nope x", json!({"pattern": "x", "type": "nope"}), "`type` \"nope\" is not a file type"),
        exact(
            "-n -U 'main\\(\\) \\{\\n.*println'",
            c("main\\(\\) \\{\\n.*println", json!({"multiline": true})),
            "src/main.rs:1:fn main() {\nsrc/main.rs:2:    println!(\"hello world\");",
        ),
        err(
            "-n 'a\\nb' (no -U)",
            c("main\\(\\) \\{\\n", json!({})),
            "set `multiline` to true",
        ),
        exact(
            "-n -U --multiline-dotall 'fn main.*?\\}'",
            c("fn main.*?\\}", json!({"multiline": true})),
            "src/main.rs:1:fn main() {\nsrc/main.rs:2:    println!(\"hello world\");\nsrc/main.rs:3:    helper();\nsrc/main.rs:4:}",
        ),
        exact(
            "-n hello | head -2",
            c("hello", json!({"head_limit": 2})),
            "README.md:2:hello docs\ndocs/guide.md:1:hello from the guide\n[showing 2 of 3; pass offset=2 for the next page]",
        ),
        exact(
            "-n hello | tail -n +2 | head -1",
            c("hello", json!({"head_limit": 1, "offset": 1})),
            "docs/guide.md:1:hello from the guide\n[showing 1 of 3; pass offset=2 for the next page]",
        ),
        exact("-n hello (head_limit 0 is all)", c("hello", json!({"head_limit": 0})), hello_all),
        err("-n '(' ", c("(", json!({})), "is not a valid regular expression"),
        err("(empty pattern)", json!({"pattern": ""}), "`pattern` is empty"),
        exact(
            "secret (in a .gitignore'd dir)",
            c("secret", json!({})),
            "no line matched `secret` in the 9 files searched",
        ),
        exact("-n -g '*.log' hello (an explicit glob beats .gitignore)", c("hello", json!({"glob": "*.log"})), "build.log:1:hello log"),
        exact(
            "secret ignored/ (named explicitly)",
            c("secret", json!({"path": "ignored"})),
            "ignored/secret.txt:1:hello secret",
        ),
        exact("-n 'cargo test' (hidden file)", c("cargo test", json!({})), ".github/ci.yml:2:run: cargo test"),
        exact("-n 'hello = 1' (.git is never searched)", c("hello = 1", json!({})), "no line matched `hello = 1` in the 9 files searched"),
        exact("-n helper src/main.rs (one file)", c("helper", json!({"path": "src/main.rs"})), "src/main.rs:3:    helper();"),
        exact("--crlf -n 'alpha$'", c("alpha$", json!({})), "crlf.txt:1:alpha"),
        exact("-n hello docs", c("hello", json!({"path": "docs"})), "docs/guide.md:1:hello from the guide"),
        exact(
            "-n -g 'util/*.rs' pub src (glob relative to path)",
            c("^pub", json!({"path": "src", "glob": "util/*.rs"})),
            "src/util/mod.rs:1:pub mod strings;\nsrc/util/strings.rs:1:pub fn shout(s: &str) -> String {",
        ),
        exact(
            "-n -g 'util/*.rs' pub src (glob that repeats path)",
            c("^pub", json!({"path": "src", "glob": "src/util/*.rs"})),
            "src/util/mod.rs:1:pub mod strings;\nsrc/util/strings.rs:1:pub fn shout(s: &str) -> String {",
        ),
        err("(unknown flag)", json!({"pattern": "x", "-z": true}), "`-z` is not a parameter of `grep`"),
        err("(bad output mode)", json!({"pattern": "x", "output_mode": "lines"}), "`output_mode` \"lines\""),
        err("-C 999", c("x", json!({"-C": 999})), "NOT reduced to fit"),
        err("(missing path)", c("x", json!({"path": "nope"})), "does not exist"),
        exact("-n '^$' (blank lines found)", c("^# ", json!({})), "README.md:1:# Demo"),
    ];
    assert!(check(&t, grep, cases) >= 40, "the fixture has 40 grep cases");
}

#[test]
fn parity_t11_binary_long_lines_and_skipped_directories() {
    // A NUL byte: searched only up to it; the result says so.
    let t = tree(&[("a.txt", b"needle here\n"), ("b.bin", b"x\0needle"), (".git/HEAD", b"ref\n")]);
    let o = grep(&env(&t), &json!({"pattern": "needle", "output_mode": "content"}));
    assert_eq!(
        o.content,
        "a.txt:1:needle here\n[1 file(s) contain a NUL byte: they were searched only up to it, as ripgrep does.]"
    );
    // A line past 500 characters is cut and says by how much (rg --max-columns omits it).
    let long = format!("{}needle{}\n", "a".repeat(498), "b".repeat(100));
    let t = tree(&[("long.txt", long.as_bytes())]);
    let o = grep(&env(&t), &json!({"pattern": "needle", "output_mode": "content"}));
    assert_eq!(o.content, format!("long.txt:1:{}ne… [line cut, 104 more characters]", "a".repeat(498)));
    // Build output is skipped unless named, and the result says so.
    let t = tree(&[("a.txt", b"hello\n"), ("node_modules/dep/index.js", b"hello dep\n"), ("target/x.txt", b"hello t\n")]);
    let o = grep(&env(&t), &json!({"pattern": "hello", "output_mode": "content"}));
    assert_eq!(
        o.content,
        "a.txt:1:hello\n[not searched, because they are build output or caches: node_modules, target. Name one as `path` to look inside it.]"
    );
    let o = grep(&env(&t), &json!({"pattern": "hello", "output_mode": "content", "path": "node_modules"}));
    assert_eq!(o.content, "node_modules/dep/index.js:1:hello dep");
    // `.ignore` applies without git; `.gitignore` only inside a git repo (rg's require_git).
    let t = tree(&[(".ignore", b"skipme.txt\n"), ("skipme.txt", b"hello\n"), ("keep.txt", b"hello\n")]);
    let o = grep(&env(&t), &json!({"pattern": "hello", "output_mode": "content"}));
    assert_eq!(o.content, "keep.txt:1:hello");
    let t = tree(&[(".gitignore", b"x.txt\n"), ("x.txt", b"hello\n"), ("y.txt", b"hello\n")]);
    let o = grep(&env(&t), &json!({"pattern": "hello", "output_mode": "content"}));
    assert_eq!(o.content, "x.txt:1:hello\ny.txt:1:hello");
    // The same files in a git repo: the .gitignore applies.
    let t = tree(&[(".git/HEAD", b"ref\n"), (".gitignore", b"x.txt\n"), ("x.txt", b"hello\n"), ("y.txt", b"hello\n")]);
    let o = grep(&env(&t), &json!({"pattern": "hello", "output_mode": "content"}));
    assert_eq!(o.content, "y.txt:1:hello");
}

#[test]
fn parity_t11_an_ordinary_tree_keeps_context_groups_apart_across_files() {
    let t = tree(&[(".git/HEAD", b"x\n"), ("a.txt", b"one\ntwo needle\nthree\nfour\nfive\nsix needle\n"), ("b.txt", b"needle\n")]);
    let o = grep(&env(&t), &json!({"pattern": "needle", "output_mode": "content", "-C": 1}));
    assert_eq!(
        o.content,
        "a.txt-1-one\na.txt:2:two needle\na.txt-3-three\n--\na.txt-5-five\na.txt:6:six needle\n--\nb.txt:1:needle"
    );
}

#[test]
fn parity_t10_glob_matches_the_rg_fixture() {
    let t = base();
    // Newest first: pin the order of the Rust files.
    t.newer("src/main.rs", 500);
    t.newer("src/util/strings.rs", 400);
    t.newer("src/lib.rs", 300);
    t.newer("src/util/mod.rs", 200);
    let rs = "src/main.rs\nsrc/util/strings.rs\nsrc/lib.rs\nsrc/util/mod.rs";
    let cases = vec![
        exact("--files -g '**/*.rs'", json!({"pattern": "**/*.rs"}), rs),
        exact("--files -g '*.rs'", json!({"pattern": "*.rs"}), rs),
        exact("--files -g 'src/*.rs'", json!({"pattern": "src/*.rs"}), "src/main.rs\nsrc/lib.rs"),
        exact("--files -g 'src/**/*.rs'", json!({"pattern": "src/**/*.rs"}), rs),
        exact("--files -g '*.{md,yml}' (hidden included)", json!({"pattern": "*.{md,yml}", "head_limit": 10}), ".github/ci.yml\nREADME.md\ndocs/guide.md"),
        exact("--files -g 'mod.rs' (a bare name matches at any depth)", json!({"pattern": "mod.rs"}), "src/util/mod.rs"),
        exact("--files -g '*.rs' src/util", json!({"pattern": "*.rs", "path": "src/util"}), "src/util/strings.rs\nsrc/util/mod.rs"),
        exact("--files -g 'util/*.rs' src (relative to path)", json!({"pattern": "util/*.rs", "path": "src"}), "src/util/strings.rs\nsrc/util/mod.rs"),
        exact("--files -g 'src\\*.rs' (backslashes)", json!({"pattern": "src\\*.rs"}), "src/main.rs\nsrc/lib.rs"),
        exact("--files -g '*.rs' | head -2", json!({"pattern": "*.rs", "head_limit": 2}), "src/main.rs\nsrc/util/strings.rs\n[showing 2 of 4; pass offset=2 for the next page]"),
        exact("--files -g '*.rs' | tail -n +3", json!({"pattern": "*.rs", "offset": 2}), "src/lib.rs\nsrc/util/mod.rs"),
        exact("--files -g '*.xyz'", json!({"pattern": "*.xyz"}), "no files matched `*.xyz` (searched .). A pattern without `/` matches names at any depth (`*.rs`); one with `/` is relative to `path` (`src/*.rs`, `**/Cargo.toml`). Hidden files count; files ignored by .gitignore do not."),
        exact("--files -g '*.log' (an explicit glob beats .gitignore, as in rg)", json!({"pattern": "*.log"}), "build.log"),
        err("(a file as path)", json!({"path": "src/main.rs"}), "`path` is a file"),
        err("(unknown flag)", json!({"pattern": "*.rs", "sort": "name"}), "`sort` is not a parameter of `glob`"),
    ];
    assert!(check(&t, glob, cases) >= 10);

    // `**/Cargo.toml` finds every manifest, the top-level one included.
    let t = tree(&[
        (".git/HEAD", b"x\n"),
        ("Cargo.toml", b""),
        ("crates/a/Cargo.toml", b""),
        ("crates/a/src/lib.rs", b""),
        ("crates/b/Cargo.toml", b""),
    ]);
    t.newer("crates/b/Cargo.toml", 30);
    t.newer("crates/a/Cargo.toml", 20);
    t.newer("Cargo.toml", 10);
    let o = glob(&env(&t), &json!({"pattern": "**/Cargo.toml"}));
    assert_eq!(o.content, "crates/b/Cargo.toml\ncrates/a/Cargo.toml\nCargo.toml");
    let o = glob(&env(&t), &json!({"pattern": "crates/*/Cargo.toml"}));
    assert_eq!(o.content, "crates/b/Cargo.toml\ncrates/a/Cargo.toml");
    // No pattern lists every file; absolute patterns under the base work.
    let o = glob(&env(&t), &json!({}));
    assert_eq!(o.content.lines().count(), 4);
    let abs = format!("{}/crates/*/Cargo.toml", t.root.to_string_lossy().replace('\\', "/"));
    let o = glob(&env(&t), &json!({"pattern": abs}));
    assert_eq!(o.content, "crates/b/Cargo.toml\ncrates/a/Cargo.toml");
}

#[test]
fn a_search_over_a_non_local_port_uses_the_ports_walk_and_read() {
    use marlowe_os::workspace::{ExecOutput, ExecRequest, Meta, WalkEntry, WalkOpts, Workspace};
    use std::path::Path;

    /// A port that is not this machine: it can only walk, read and stat.
    struct Remote(PathBuf);
    impl Workspace for Remote {
        fn kind(&self) -> String {
            "docker:test".into()
        }
        fn read(&self, p: &Path) -> std::io::Result<Vec<u8>> {
            std::fs::read(p)
        }
        fn write(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        fn walk(&self, d: &Path, o: &WalkOpts) -> std::io::Result<Vec<WalkEntry>> {
            LocalFs.walk(d, o)
        }
        fn metadata(&self, p: &Path) -> std::io::Result<Meta> {
            LocalFs.metadata(p)
        }
        fn exec(&self, _: &ExecRequest, _: &dyn Fn() -> bool) -> std::io::Result<ExecOutput> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }
    let t = base();
    let port = Remote(t.root.clone());
    let e = SearchEnv { root: Some(&port.0), port: &port };
    let o = grep(&e, &json!({"pattern": "hello", "output_mode": "content", "glob": "*.md"}));
    assert_eq!(o.content, "README.md:2:hello docs\ndocs/guide.md:1:hello from the guide");
    let o = glob(&e, &json!({"pattern": "src/*.rs", "head_limit": 10}));
    assert_eq!(o.content.lines().count(), 2, "{}", o.content);
}

/// The installed ripgrep, if there is one: `rg` on PATH, else the `rg` mode of the Claude Code
/// binary (it embeds ripgrep and answers to `ARGV0=rg`). `None` skips the oracle test.
fn find_rg() -> Option<(std::ffi::OsString, bool)> {
    let works = |prog: &std::ffi::OsStr, argv0: bool| {
        rg_output(prog, argv0, std::env::temp_dir().as_path(), &["--version"]).is_some_and(|o| o.starts_with("ripgrep"))
    };
    if let Ok(p) = std::env::var("MARLOWE_RG") {
        return works(p.as_ref(), false).then(|| (p.into(), false));
    }
    if works("rg".as_ref(), false) {
        return Some(("rg".into(), false));
    }
    let mut cands: Vec<std::ffi::OsString> = Vec::new();
    cands.extend(std::env::var_os("CLAUDE_CODE_EXECPATH"));
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        cands.push(PathBuf::from(home).join(".local").join("bin").join("claude.exe").into_os_string());
    }
    cands.into_iter().find(|p| works(p, true)).map(|p| (p, true))
}

/// stdout of a ripgrep run (through `marlowe_os::child`, like every child here); `None` when it
/// cannot start. Exit 1 (no match) is still output.
fn rg_output(prog: &std::ffi::OsStr, argv0: bool, cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    use marlowe_os::child::{run, ChildSpec, RunLimits};
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args);
    let mut spec = ChildSpec::new().with_cwd(cwd);
    if argv0 {
        spec = spec.with_env([("ARGV0".to_string(), "rg".to_string())]);
    }
    let limits = RunLimits { timeout: Duration::from_secs(30), max_output_bytes: 8 << 20 };
    let out = run(cmd, &spec, limits, &|| false).ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A.9 against the real thing: the cases whose answer is exactly ripgrep's are run through the
/// installed `rg` and compared line for line (the deliberate differences in `search/mod.rs` are
/// not among them). Skipped, loudly, where ripgrep is not installed.
#[test]
fn parity_t11_the_fixture_agrees_with_the_installed_ripgrep() {
    let Some((rg, argv0)) = find_rg() else {
        eprintln!("skipped: no ripgrep on PATH (set MARLOWE_RG to its path)");
        return;
    };
    let t = base();
    let e = env(&t);
    let norm = |s: &str| s.replace("\r\n", "\n").replace('\\', "/").trim_end().to_string();
    let run_rg = |args: &[&str]| -> String {
        let mut all = vec!["--hidden", "--glob", "!.git", "--sort", "path", "--no-heading", "--color", "never", "-H"];
        all.extend_from_slice(args);
        norm(&rg_output(&rg, argv0, &t.root, &all).expect("rg runs"))
    };
    let sorted = |s: &str| {
        let mut v: Vec<&str> = s.lines().collect();
        v.sort_unstable();
        v.join("\n")
    };
    // (rg arguments after the base flags, the grep call)
    let content: Vec<(Vec<&str>, Value)> = vec![
        (vec!["-n", "hello"], json!({"pattern": "hello"})),
        (vec!["-n", "-i", "todo"], json!({"pattern": "todo", "-i": true})),
        (vec!["-n", "todo"], json!({"pattern": "todo"})),
        (vec!["-n", "-C", "1", r"helper\(\)", "src"], json!({"pattern": r"helper\(\)", "-C": 1, "path": "src"})),
        (vec!["-n", "-A", "1", "fn main", "src/main.rs"], json!({"pattern": "fn main", "-A": 1, "path": "src/main.rs"})),
        (vec!["-n", "-B", "1", r"helper\(\);", "src/main.rs"], json!({"pattern": r"helper\(\);", "-B": 1, "path": "src/main.rs"})),
        (vec!["-n", "-A", "1", "^pub fn", "src/lib.rs"], json!({"pattern": "^pub fn", "-A": 1, "path": "src/lib.rs"})),
        (vec!["-n", "-g", "*.md", "hello"], json!({"pattern": "hello", "glob": "*.md"})),
        (vec!["-n", "-g", "*.{md,rs}", "hello"], json!({"pattern": "hello", "glob": "*.{md,rs}"})),
        (vec!["-n", "-g", "docs/*.md", "hello"], json!({"pattern": "hello", "glob": "docs/*.md"})),
        (vec!["-n", "-g", "!*.md", "hello"], json!({"pattern": "hello", "glob": "!*.md"})),
        (vec!["-n", "-g", "*.log", "hello"], json!({"pattern": "hello", "glob": "*.log"})),
        (vec!["-n", "secret", "ignored"], json!({"pattern": "secret", "path": "ignored"})),
        (vec!["-n", "cargo test"], json!({"pattern": "cargo test"})),
        (vec!["-n", "--crlf", "alpha$"], json!({"pattern": "alpha$"})),
        (vec!["-n", "-U", r"main\(\) \{\n.*println"], json!({"pattern": r"main\(\) \{\n.*println", "multiline": true})),
        (vec!["-n", "-U", "--multiline-dotall", r"fn main.*?\}"], json!({"pattern": r"fn main.*?\}", "multiline": true})),
        (vec!["-n", "-t", "rust", "pub fn"], json!({"pattern": "pub fn", "type": "rust"})),
        (vec!["-n", "-t", "md", "hello"], json!({"pattern": "hello", "type": "md"})),
        (vec!["-n", "secret"], json!({"pattern": "secret"})),
    ];
    let mut differ = Vec::new();
    for (rg_args, mut call) in content.clone() {
        call["output_mode"] = json!("content");
        let want = run_rg(&rg_args);
        let got = grep(&e, &call);
        let got = norm(&got.content);
        let same = if want.is_empty() { got.starts_with("no ") } else { got == want };
        if !same {
            differ.push(format!("rg {rg_args:?}\n  rg:   {want:?}\n  tool: {got:?}"));
        }
    }
    // Counts and file lists (rg sorts by path; the tool lists files newest first, so as sets).
    for (rg_args, mut call) in [
        (vec!["-c", "hello"], json!({"pattern": "hello"})),
        (vec!["-c", "-t", "rust", "fn"], json!({"pattern": "fn", "type": "rust"})),
        (vec!["-c", "-i", "TODO"], json!({"pattern": "TODO", "-i": true})),
    ] {
        call["output_mode"] = json!("count");
        let (want, got) = (run_rg(&rg_args), norm(&grep(&e, &call).content));
        if want != got {
            differ.push(format!("rg {rg_args:?}\n  rg:   {want:?}\n  tool: {got:?}"));
        }
    }
    for (rg_args, call) in [
        (vec!["-l", "hello"], json!({"pattern": "hello"})),
        (vec!["-l", "-i", "helper"], json!({"pattern": "helper", "-i": true})),
        (vec!["-l", "-g", "*.rs", "fn"], json!({"pattern": "fn", "glob": "*.rs"})),
    ] {
        let (want, got) = (sorted(&run_rg(&rg_args)), sorted(&norm(&grep(&e, &call).content)));
        if want != got {
            differ.push(format!("rg {rg_args:?}\n  rg:   {want:?}\n  tool: {got:?}"));
        }
    }
    for (pat, path) in [("*.rs", None), ("src/*.rs", None), ("**/*.rs", None), ("mod.rs", None), ("*.{md,yml}", None), ("*.log", None), ("*.rs", Some("src/util"))] {
        let mut rg_args = vec!["--files", "-g", pat];
        let mut call = json!({"pattern": pat, "head_limit": 0});
        if let Some(p) = path {
            rg_args.push(p);
            call["path"] = json!(p);
        }
        let (want, got) = (sorted(&run_rg(&rg_args)), sorted(&norm(&glob(&e, &call).content)));
        if want != got {
            differ.push(format!("rg {rg_args:?}\n  rg:   {want:?}\n  tool: {got:?}"));
        }
    }
    assert!(differ.is_empty(), "{} case(s) differ from the installed ripgrep:\n{}", differ.len(), differ.join("\n"));
}
