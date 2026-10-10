//! The client, manager and name-based engine against the in-memory fake server; the live
//! servers (rust-analyzer, pyright, typescript-language-server) and the A8 numbers are
//! `#[ignore]`d tests run by hand (`cargo test -p marlowe-code -- --ignored live_ a8_`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_os::workspace::{ExecOutput, ExecRequest, LocalFs, Meta, WalkEntry, WalkOpts, Workspace};

use super::fake::{FakeConfig, FakeLauncher};
use super::intel::{CodeIntel, IntelError, Source};
use super::manager::LspManager;
use super::*;
use crate::index::tests::Tmp;
use crate::index::{Index, Lang};

const NEVER: &dyn Fn() -> bool = &|| false;

fn fake_manager(cfg: FakeConfig) -> (FakeLauncher, LspManager) {
    let launcher = FakeLauncher::new(cfg);
    let specs = vec![
        FakeLauncher::spec("fake-rust", &[Lang::Rust], &["Cargo.toml"]),
        FakeLauncher::spec("fake-py", &[Lang::Python], &["pyproject.toml"]),
        FakeLauncher::spec("fake-ts", &[Lang::TypeScript], &["package.json"]),
    ];
    let mgr = LspManager::with(Arc::new(launcher.clone()), specs);
    (launcher, mgr)
}

fn project() -> Tmp {
    let t = Tmp::new("lsp");
    t.put("Cargo.toml", "[package]\nname = \"demo\"\n");
    t.put(
        "src/engine.rs",
        "pub struct Engine { n: u32 }\nimpl Engine {\n    pub fn start(&self) -> u32 {\n        self.n\n    }\n}\n",
    );
    t.put("src/main.rs", "fn main() {\n    let e = make();\n    e.start();\n    e.start();\n}\nfn make() -> Engine { todo!() }\n");
    t.put("src/other.rs", "pub fn start() {}\n");
    t
}

fn engine(t: &Tmp, mgr: LspManager) -> CodeIntel {
    let idx = Index::in_memory(&t.0).unwrap();
    idx.refresh(&LocalFs).unwrap();
    CodeIntel::new(&t.0, idx, mgr)
}

#[test]
fn the_client_handshakes_syncs_and_waits_for_settled_diagnostics() {
    let t = project();
    let (log, mgr) = fake_manager(FakeConfig::default());
    let f = t.put("src/bad.rs", "fn ok() {}\n// ERROR: first\n");
    let c = mgr.client(&f, &t.0).unwrap();
    assert_eq!(c.id(), "fake-rust");
    assert!(c.has("renameProvider") && !c.has("nopeProvider"));
    let (v, gen) = c.sync(&f, "fn ok() {}\n// ERROR: first\n");
    assert_eq!(v, 1);
    let d = c.wait_diagnostics(&f, gen, v, Duration::from_millis(50), Duration::from_secs(5), NEVER).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].line, d[0].severity, d[0].message.as_str()), (1, 1, "first"));

    // Same text: nothing sent. New text: a change and a save, and only a publish for the new
    // version counts.
    assert_eq!(c.sync(&f, "fn ok() {}\n// ERROR: first\n").0, 1);
    let (v2, gen2) = c.sync(&f, "fn ok() {}\n// WARN: w\n// ERROR: a\n// ERROR: b\n");
    assert_eq!(v2, 2);
    let d = c.wait_diagnostics(&f, gen2, v2, Duration::from_millis(50), Duration::from_secs(5), NEVER).unwrap();
    assert_eq!(d.iter().filter(|d| d.severity == 1).count(), 2);
    let methods = log.log();
    for m in ["initialize", "initialized", "textDocument/didOpen", "textDocument/didChange", "textDocument/didSave"] {
        assert!(methods.iter().any(|x| x == m), "{m} in {methods:?}");
    }
    assert_eq!(methods.iter().filter(|m| *m == "textDocument/didChange").count(), 1);

    // Nothing new: the wait ends at its limit with nothing.
    let started = Instant::now();
    assert!(c.wait_diagnostics(&f, u64::MAX - 1, 9, Duration::from_millis(10), Duration::from_millis(200), NEVER).is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    c.shutdown();
    assert!(!c.alive());
    assert!(log.log().iter().any(|m| m == "shutdown") && log.log().iter().any(|m| m == "exit"));
}

#[test]
fn the_manager_starts_lazily_keeps_two_live_and_stops_idle_servers() {
    let t = project();
    let launcher = FakeLauncher::new(FakeConfig::default());
    let specs = vec![
        FakeLauncher::spec("fake-rust", &[Lang::Rust], &["Cargo.toml"]),
        FakeLauncher::spec("fake-py", &[Lang::Python], &[]),
        FakeLauncher::spec("fake-ts", &[Lang::TypeScript], &[]),
    ];
    let mgr = LspManager::with_limits(Arc::new(launcher.clone()), specs, 2, Duration::from_millis(400));
    let rs = t.0.join("src/engine.rs");
    assert!(mgr.running(&rs, &t.0).is_none(), "nothing starts until asked");
    let a = mgr.client(&rs, &t.0).unwrap();
    let again = mgr.client(&rs, &t.0).unwrap();
    assert_eq!(launcher.launches(), ["fake-rust"], "one server per (server, root)");
    drop(again);
    std::thread::sleep(Duration::from_millis(20));
    let _py = mgr.client(&t.put("a.py", "def f():\n  pass\n"), &t.0).unwrap();
    let _ts = mgr.client(&t.put("a.ts", "function g() {}\n"), &t.0).unwrap();
    assert_eq!(launcher.launches(), ["fake-rust", "fake-py", "fake-ts"]);
    assert_eq!(mgr.live().len(), 2, "the least recently used server made room");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!a.alive(), "the evicted rust server was stopped");
    assert!(mgr.running(&rs, &t.0).is_none());

    // Idle servers are stopped by the reaper.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !mgr.live().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(mgr.live().is_empty(), "idle servers stop");
    // No server for the language, and no marker for an edit: nothing starts.
    assert!(matches!(mgr.client(&t.put("x.go", "package x\n"), &t.0), Err(LspError::NoServer(_))));
    let stray = Tmp::new("stray");
    assert!(mgr.for_edit(&stray.put("x.rs", "fn a() {}\n"), &stray.0).is_none());
}

#[test]
fn a_server_that_fails_to_start_is_reported_and_not_retried() {
    let t = project();
    let (log, mgr) = fake_manager(FakeConfig { fail_init: true, ..FakeConfig::default() });
    let f = t.0.join("src/engine.rs");
    let e = mgr.client(&f, &t.0).unwrap_err();
    assert!(e.to_string().contains("initialize failed"), "{e}");
    assert!(mgr.client(&f, &t.0).is_err());
    assert_eq!(log.launches().len(), 1);
    // The engine falls back to the index for references and hover.
    let eng = engine(&t, mgr);
    let (target, refs, source) = eng.references(&LocalFs, "Engine::start", None, 50, NEVER).unwrap();
    assert_eq!(source, Source::Index);
    assert_eq!(target.container.as_deref(), Some("Engine"));
    assert!(refs.iter().all(|l| l.text.contains("start")), "{refs:?}");
    let (_, hover, source) = eng.hover(&LocalFs, "Engine::start", None, NEVER).unwrap();
    assert_eq!(source, Source::Index);
    assert!(hover.contains("pub fn start(&self) -> u32"), "{hover}");
    let e = eng.plan_rename(&LocalFs, "Engine::start", "run", None, NEVER).unwrap_err();
    assert!(e.to_string().contains("needs a language server"), "{e}");
}

#[test]
fn readiness_waits_for_server_status() {
    let t = project();
    let launcher = FakeLauncher::new(FakeConfig { status_after: Some(Duration::from_millis(300)), ..FakeConfig::default() });
    let mut spec = FakeLauncher::spec("fake-rust", &[Lang::Rust], &[]);
    spec.server_status = true;
    let mgr = LspManager::with(Arc::new(launcher), vec![spec]);
    let c = mgr.client(&t.0.join("src/engine.rs"), &t.0).unwrap();
    assert!(!c.wait_ready(Duration::from_millis(50), NEVER), "not quiescent yet");
    let started = Instant::now();
    assert!(c.wait_ready(Duration::from_secs(5), NEVER));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn parity_t25_names_resolve_through_the_index_and_the_server() {
    let t = project();
    let (_log, mgr) = fake_manager(FakeConfig::default());
    let eng = engine(&t, mgr);

    // Ambiguous by bare name; `Container::name` and `path` narrow it.
    let e = eng.one(&LocalFs, "start", None, NEVER).unwrap_err();
    assert!(matches!(e, IntelError::Ambiguous(_)), "{e}");
    assert!(e.to_string().contains("src/engine.rs:3") && e.to_string().contains("src/other.rs:1"), "{e}");
    let target = eng.one(&LocalFs, "Engine::start", None, NEVER).unwrap();
    assert_eq!((target.line, target.end_line), (3, 5));
    assert_eq!(eng.one(&LocalFs, "other::start", None, NEVER).unwrap().path, t.0.join("src").join("other.rs"));
    let body = eng.body(&LocalFs, &target);
    assert_eq!(body.len(), 3);
    assert_eq!(body[1], (4, "        self.n".to_string()));

    // A name only used in `path` resolves through the server's go-to-definition from the use.
    let (found, source) = eng.resolve(&LocalFs, "Engine", Some(&t.0.join("src/main.rs")), None, NEVER).unwrap();
    assert_eq!(source, Source::Server);
    assert_eq!(found[0].path, t.0.join("src").join("engine.rs"));
    assert_eq!(found[0].kind, "class", "the index fills in what it knows");

    // References and hover come from the server at the resolved definition.
    let (_, refs, source) = eng.references(&LocalFs, "Engine::start", None, 50, NEVER).unwrap();
    assert_eq!(source, Source::Server);
    let lines: Vec<(String, u32)> = refs.iter().map(|l| (eng.rel(&l.path), l.line)).collect();
    // The fake does not tell same-named symbols apart; a real server does. The declaration is
    // left out.
    assert!(lines.contains(&("src/main.rs".into(), 3)) && lines.contains(&("src/main.rs".into(), 4)), "{lines:?}");
    assert!(!lines.contains(&("src/engine.rs".into(), 3)), "{lines:?}");
    let (_, hover, source) = eng.hover(&LocalFs, "Engine::start", None, NEVER).unwrap();
    assert_eq!(source, Source::Server);
    assert!(hover.contains("fake docs for start"), "{hover}");

    // Symbols: by query over the index, or a file's own.
    let (hits, _) = eng.symbols(&LocalFs, "eng", None, None, 10, NEVER).unwrap();
    assert_eq!(hits[0].name, "Engine");
    let (hits, _) = eng.symbols(&LocalFs, "", Some(&t.0.join("src/engine.rs")), None, 10, NEVER).unwrap();
    assert_eq!(hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["Engine", "start"]);
    assert!(matches!(eng.one(&LocalFs, "nope_nothing", None, NEVER), Err(IntelError::NotFound(_))));
}

#[test]
fn a_name_the_index_does_not_have_comes_from_workspace_symbols() {
    let t = project();
    t.put(".gitignore", "gen/\n");
    t.put("gen/hidden.rs", "pub fn generated_entry() {}\n");
    let (_log, mgr) = fake_manager(FakeConfig::default());
    let eng = engine(&t, mgr);
    assert!(eng.index().defs("generated_entry").is_empty());
    let (found, source) = eng.resolve(&LocalFs, "generated_entry", None, None, NEVER).unwrap();
    assert_eq!(source, Source::Server);
    assert_eq!(found[0].path, t.0.join("gen").join("hidden.rs"));
    assert_eq!(found[0].kind, "function");
}

#[test]
fn parity_t26_rename_applies_every_file_atomically() {
    for (utf8, document_changes) in [(false, false), (true, true)] {
        let t = project();
        t.put("src/wide.rs", "// é😀 start\nfn wide() { let s = \"é😀\"; s.start(); }\n");
        let (_log, mgr) = fake_manager(FakeConfig { utf8, document_changes, ..FakeConfig::default() });
        let eng = engine(&t, mgr);
        let plan = eng.plan_rename(&LocalFs, "Engine::start", "launch", None, NEVER).unwrap();
        let rels: Vec<String> = plan.files.iter().map(|f| eng.rel(&f.path)).collect();
        assert!(rels.contains(&"src/engine.rs".into()) && rels.contains(&"src/main.rs".into()), "{rels:?}");
        assert!(plan.files.iter().all(|f| f.diff.starts_with("--- a/src/")), "root-relative diff labels");
        // Nothing is written until the plan is applied.
        assert!(std::fs::read_to_string(t.0.join("src/main.rs")).unwrap().contains("e.start()"));
        eng.apply_rename(&LocalFs, &plan).unwrap();
        let main = std::fs::read_to_string(t.0.join("src/main.rs")).unwrap();
        assert_eq!(main.matches("e.launch()").count(), 2, "{main}");
        let wide = std::fs::read_to_string(t.0.join("src/wide.rs")).unwrap();
        assert_eq!(wide, "// é😀 launch\nfn wide() { let s = \"é😀\"; s.launch(); }\n", "positions after astral chars (utf8={utf8})");
        assert!(eng.index().defs("start").iter().all(|s| !s.path.ends_with("engine.rs")), "the index saw the rename");
        assert!(eng.index().defs("launch").iter().any(|s| s.path.ends_with("engine.rs")));
        // Stale plan: refused, nothing written.
        let plan = eng.plan_rename(&LocalFs, "Engine::launch", "go", None, NEVER).unwrap();
        std::fs::write(t.0.join("src/main.rs"), format!("{main}// touched\n")).unwrap();
        let e = eng.apply_rename(&LocalFs, &plan).unwrap_err();
        assert!(e.to_string().contains("changed since"), "{e}");
        assert!(std::fs::read_to_string(t.0.join("src/engine.rs")).unwrap().contains("fn launch"));
    }
}

/// A workspace whose `n`th write fails.
struct FailingWrites {
    fail_at: usize,
    writes: AtomicUsize,
}

impl Workspace for FailingWrites {
    fn kind(&self) -> String {
        "failing".into()
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        LocalFs.read(path)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if self.writes.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at {
            return Err(std::io::Error::other("disk full"));
        }
        LocalFs.write(path, bytes)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        LocalFs.rename(from, to)
    }
    fn walk(&self, dir: &Path, opts: &WalkOpts) -> std::io::Result<Vec<WalkEntry>> {
        LocalFs.walk(dir, opts)
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Meta> {
        LocalFs.metadata(path)
    }
    fn exec(&self, req: &ExecRequest, cancelled: &dyn Fn() -> bool) -> std::io::Result<ExecOutput> {
        LocalFs.exec(req, cancelled)
    }
}

#[test]
fn a_failed_write_puts_every_file_back() {
    let t = project();
    let (_log, mgr) = fake_manager(FakeConfig::default());
    let eng = engine(&t, mgr);
    let before: Vec<(PathBuf, String)> = ["src/engine.rs", "src/main.rs", "src/other.rs"]
        .iter()
        .map(|r| (t.0.join(r), std::fs::read_to_string(t.0.join(r)).unwrap()))
        .collect();
    let plan = eng.plan_rename(&LocalFs, "Engine::start", "launch", None, NEVER).unwrap();
    assert!(plan.files.len() >= 2);
    let ws = FailingWrites { fail_at: 2, writes: AtomicUsize::new(0) };
    let e = eng.apply_rename(&ws, &plan).unwrap_err();
    assert!(e.to_string().contains("put back"), "{e}");
    for (p, text) in before {
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text, "{} restored", p.display());
    }
}

#[test]
fn rename_refuses_edits_outside_the_root_and_bad_names() {
    let t = project();
    let (_log, mgr) = fake_manager(FakeConfig::default());
    let eng = engine(&t, mgr);
    assert!(matches!(eng.plan_rename(&LocalFs, "Engine::start", "not-a-name", None, NEVER), Err(IntelError::Refused(_))));
    assert!(matches!(eng.plan_rename(&LocalFs, "Engine::start", "start", None, NEVER), Err(IntelError::Refused(_))));
    // An engine whose root is a subfolder: the server's edit reaches outside it.
    let sub = t.0.join("src");
    let (_log, mgr) = fake_manager(FakeConfig::default());
    let idx = Index::in_memory(&sub).unwrap();
    idx.refresh(&LocalFs).unwrap();
    let narrow = CodeIntel::new(&sub, idx, mgr);
    t.put("outside.rs", "fn use_it() { Engine::start(); }\n");
    // The fake reads files under its root only, so plant the outside use through a document
    // the server is told about.
    let c = narrow.lsp().client(&sub.join("engine.rs"), &sub).unwrap();
    c.sync(&t.0.join("outside.rs"), "fn use_it() { x.start(); }\n");
    let e = narrow.plan_rename(&LocalFs, "Engine::start", "launch", None, NEVER).unwrap_err();
    assert!(e.to_string().contains("outside the workspace"), "{e}");
    assert!(std::fs::read_to_string(sub.join("engine.rs")).unwrap().contains("fn start"));
}

#[test]
fn parity_t24_new_errors_after_an_edit_are_reported_and_old_ones_are_not() {
    for pull in [false, true] {
        let t = project();
        let f = t.put("src/lib.rs", "fn a() {}\n// ERROR: old problem\n");
        let (_log, mgr) = fake_manager(FakeConfig { pull, ..FakeConfig::default() });
        let eng = engine(&t, mgr);
        // The edit follows at once: the pre-edit publish has not landed yet, and after_edit
        // waits for it before sending the change.
        let snap = eng.before_edit(&LocalFs, std::slice::from_ref(&f));
        assert_eq!(snap.len(), 1);
        std::fs::write(&f, "fn a() {}\n// ERROR: old problem\nfn b() {}\n// ERROR: new problem\n// WARN: meh\n").unwrap();
        let started = Instant::now();
        let out = eng.after_edit(&LocalFs, std::slice::from_ref(&f), &snap, NEVER);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        let d = &out[0];
        assert_eq!(d.new_errors.len(), 1, "pull={pull}: {d:?}");
        assert_eq!(d.new_errors[0].message, "new problem");
        assert_eq!(d.new_errors[0].line, 3);
        assert_eq!((d.errors, d.warnings), (2, 1));
        assert_eq!(d.lines[0], "// ERROR: new problem");
        assert_eq!(eng.index().defs("b").len(), 1, "the edit was re-tagged");
    }
}

#[test]
fn an_edit_outside_a_project_starts_no_server() {
    let t = Tmp::new("noproj");
    let f = t.put("a.rs", "fn a() {}\n// ERROR: x\n");
    let (log, mgr) = fake_manager(FakeConfig::default());
    let eng = CodeIntel::new(&t.0, Index::in_memory(&t.0).unwrap(), mgr);
    let snap = eng.before_edit(&LocalFs, std::slice::from_ref(&f));
    assert!(eng.after_edit(&LocalFs, std::slice::from_ref(&f), &snap, NEVER).is_empty());
    assert!(log.launches().is_empty());
    // The diagnostics tool starts it on request.
    let d = eng.diagnostics(&LocalFs, Some(&f), NEVER).unwrap();
    assert_eq!(d[0].1.len(), 1);
    assert_eq!(log.launches(), ["fake-rust"]);
    let all = eng.diagnostics(&LocalFs, None, NEVER).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, f);
}

// ---- live servers (run by hand) -----------------------------------------------------------

/// A real server on a scratch project: definition, references, hover, rename and post-edit
/// diagnostics, as the tools use them.
fn live_roundtrip(files: &[(&str, &str)], file: &str, symbol: &str, use_file: &str, broken: &str, expect_error: &str) {
    let t = Tmp::new("live");
    for (p, text) in files {
        t.put(p, text);
    }
    let mgr = LspManager::new();
    let started = Instant::now();
    let c = mgr.client(&t.0.join(file), &t.0).unwrap_or_else(|e| panic!("{e}"));
    let ready = c.wait_ready(super::intel::READY_MAX, NEVER);
    eprintln!("{} up in {:?} (ready: {ready})", c.id(), started.elapsed());
    let eng = engine(&t, mgr.clone());
    let target = eng.one(&LocalFs, symbol, None, NEVER).unwrap();
    assert!(target.path.ends_with(file), "{target:?}");
    let (_, refs, source) = eng.references(&LocalFs, symbol, None, 50, NEVER).unwrap();
    eprintln!("references at {:?}: {refs:?}", started.elapsed());
    assert_eq!(source, Source::Server);
    assert!(refs.iter().any(|l| l.path.ends_with(use_file)), "{refs:?}");
    let (_, hover, source) = eng.hover(&LocalFs, symbol, None, NEVER).unwrap();
    assert_eq!(source, Source::Server, "{hover}");
    let plan = eng.plan_rename(&LocalFs, symbol, "renamed_thing", None, NEVER).unwrap();
    eng.apply_rename(&LocalFs, &plan).unwrap();
    let used = std::fs::read_to_string(t.0.join(use_file)).unwrap();
    assert!(used.contains("renamed_thing"), "{used}");
    let f = t.0.join(file);
    let snap = eng.before_edit(&LocalFs, std::slice::from_ref(&f));
    let text = std::fs::read_to_string(&f).unwrap();
    std::fs::write(&f, format!("{text}\n{broken}\n")).unwrap();
    let mut found = Vec::new();
    for _ in 0..6 {
        found = eng.after_edit(&LocalFs, std::slice::from_ref(&f), &snap, NEVER);
        if found.iter().any(|d| !d.new_errors.is_empty()) {
            break;
        }
    }
    eprintln!("after edit: {found:?}; pulls: {}", c.pulls_diagnostics());
    let msgs: Vec<String> = found.iter().flat_map(|d| d.new_errors.iter().map(|e| e.message.clone())).collect();
    assert!(msgs.iter().any(|m| m.contains(expect_error)), "{msgs:?}");
    mgr.shutdown_all();
}

#[test]
#[ignore = "needs rust-analyzer; run by hand"]
fn live_rust_analyzer() {
    live_roundtrip(
        &[
            ("Cargo.toml", "[package]\nname = \"live\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", "pub mod engine;\npub fn run() -> u32 { engine::Engine::default().start() }\n"),
            ("src/engine.rs", "#[derive(Default)]\npub struct Engine { n: u32 }\nimpl Engine {\n    pub fn start(&self) -> u32 { self.n }\n}\n"),
        ],
        "src/engine.rs",
        "Engine::start",
        "src/lib.rs",
        "fn broken() -> u32 { \"text\" }",
        "expected u32",
    );
}

#[test]
#[ignore = "needs pyright-langserver; run by hand"]
fn live_pyright() {
    live_roundtrip(
        &[
            ("pyproject.toml", "[project]\nname = \"live\"\n"),
            ("pkg/__init__.py", ""),
            ("pkg/engine.py", "class Engine:\n    def start(self) -> int:\n        return 1\n"),
            ("pkg/use.py", "from pkg.engine import Engine\n\ndef run() -> int:\n    return Engine().start()\n"),
        ],
        "pkg/engine.py",
        "Engine.start",
        "pkg/use.py",
        "def broken() -> int:\n    return undefined_name_here",
        "undefined_name_here",
    );
}

#[test]
#[ignore = "needs typescript-language-server; run by hand"]
fn live_typescript() {
    live_roundtrip(
        &[
            ("package.json", "{\"name\": \"live\", \"version\": \"0.0.0\"}\n"),
            ("tsconfig.json", "{\"compilerOptions\": {\"strict\": true, \"target\": \"es2020\", \"module\": \"commonjs\"}}\n"),
            ("src/engine.ts", "export class Engine {\n  start(): number {\n    return 1;\n  }\n}\n"),
            ("src/use.ts", "import { Engine } from \"./engine\";\nexport function run(): number {\n  return new Engine().start();\n}\n"),
        ],
        "src/engine.ts",
        "Engine.start",
        "src/use.ts",
        "export const broken: number = \"text\";",
        "not assignable",
    );
}
