//! The code group through the tool host, against the in-memory fake language server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_agent::{CallCtx, Consequence, Exposure, Mutation, SandboxView, ToolCtx, ToolHost, ToolOutcome};
use marlowe_code::index::Lang;
use marlowe_code::lsp::fake::{FakeConfig, FakeLauncher};
use marlowe_code::lsp::LspManager;
use marlowe_model::{CancelToken, ToolCall};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::*;

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("marlowe-codetools-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Ws(p)
    }

    fn put(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn get(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn project() -> Ws {
    let w = Ws::new("proj");
    w.put("Cargo.toml", "[package]\nname = \"demo\"\n");
    w.put(
        "src/engine.rs",
        "/// Runs things.\npub struct Engine { n: u32 }\nimpl Engine {\n    pub fn start(&self) -> u32 {\n        self.n\n    }\n}\n",
    );
    w.put("src/main.rs", "fn main() {\n    let e = make();\n    e.start();\n}\nfn make() -> Engine { todo!() }\n");
    w.put("src/other.rs", "pub fn start() {}\n");
    w
}

fn host(w: &Ws, cfg: FakeConfig) -> (FakeLauncher, BuiltinTools) {
    let launcher = FakeLauncher::new(cfg);
    let lsp = LspManager::with(Arc::new(launcher.clone()), vec![FakeLauncher::spec("fake-rust", &[Lang::Rust], &["Cargo.toml"])]);
    let tools = BuiltinTools::new(Some(w.0.clone())).with_state(CodeHost::new(None, lsp));
    (launcher, tools)
}

fn ctx(root: &Path) -> ToolCtx {
    ToolCtx { session: SessionId("s".into()), turn: 1, workspace: Some(root.to_path_buf()), cancel: CancelToken::new(), tainted: false }
}

fn call(t: &BuiltinTools, name: &str, args: Value) -> ToolOutcome {
    t.call(&ToolCall { id: "c1".into(), name: name.into(), args }, &ctx(t.workspace().unwrap()))
}

#[test]
fn the_group_registers_six_tools_offered_only_with_a_workspace() {
    let names: Vec<String> = tools().iter().map(|d| d.name().to_string()).collect();
    assert_eq!(names, ["symbols", "definition", "references", "hover", "diagnostics", "rename_symbol"]);
    for d in tools() {
        assert_eq!(d.spec.input_schema["additionalProperties"], json!(false));
        assert_eq!(d.manifest.group, ToolGroup::Code);
    }
    let w = project();
    let with_root = BuiltinTools::new(Some(w.0.clone()));
    assert!(with_root.specs().iter().any(|s| s.name == "rename_symbol"));
    assert!(!BuiltinTools::new(None).specs().iter().any(|s| s.name == "symbols"), "no workspace, no code tools");
    assert!(detect(Some(&w.0)) && !detect(None) && !detect(Some(&w.0.join("src"))));
    assert_eq!(middleware(&with_root).len(), 1);
    assert!(middleware(&BuiltinTools::new(None)).is_empty());
}

#[test]
fn parity_t25_symbols_definition_references_and_hover_resolve_by_name() {
    let w = project();
    let (_l, t) = host(&w, FakeConfig::default());

    let o = call(&t, "symbols", json!({"query": "start"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("src/engine.rs:4  pub fn start(&self) -> u32  [method in Engine]"), "{}", o.content);
    assert!(o.content.contains("src/other.rs:1  pub fn start() {}") || o.content.contains("src/other.rs:1  pub fn start() }"), "{}", o.content);
    let o = call(&t, "symbols", json!({"path": "src/engine.rs"}));
    assert!(o.content.contains("Engine") && o.content.contains("start"), "{}", o.content);
    assert!(call(&t, "symbols", json!({})).is_error);

    let o = call(&t, "definition", json!({"symbol": "Engine::start"}));
    assert!(o.content.starts_with("Engine::start  (method in Engine)  src/engine.rs:4"), "{}", o.content);
    assert!(o.content.contains("     5│         self.n"), "the body is shown:\n{}", o.content);
    let o = call(&t, "definition", json!({"symbol": "start"}));
    assert!(o.content.starts_with("2 definitions of `start`"), "{}", o.content);
    let o = call(&t, "definition", json!({"symbol": "Engine", "path": "src/main.rs"}));
    assert!(o.content.contains("src/engine.rs:2") && o.content.contains("pub struct Engine"), "{}", o.content);

    let o = call(&t, "references", json!({"symbol": "Engine::start"}));
    assert!(o.content.contains("(language server)"), "{}", o.content);
    assert!(o.content.contains("src/main.rs\n     3:7  │ e.start();"), "{}", o.content);
    let o = call(&t, "references", json!({"symbol": "start"}));
    assert!(o.is_error && o.content.contains("names 2 definitions"), "{}", o.content);

    let o = call(&t, "hover", json!({"symbol": "Engine::start"}));
    assert!(o.content.contains("fake docs for start") && o.content.contains("(language server)"), "{}", o.content);
    let o = call(&t, "definition", json!({"symbol": "missing_thing"}));
    assert!(o.is_error && o.content.contains("no definition of `missing_thing`"), "{}", o.content);
}

#[test]
fn parity_t26_rename_symbol_edits_every_file_atomically_and_shows_the_diff() {
    let w = project();
    let (_l, t) = host(&w, FakeConfig::default());
    let call_req = ToolCall { id: "r".into(), name: "rename_symbol".into(), args: json!({"symbol": "Engine::start", "new_name": "launch"}) };
    assert_eq!(t.consequence(&call_req), Consequence::Free, "a rename inside the workspace is as free as an edit");
    assert_eq!(t.mutation(&call_req, &ctx(&w.0)), Mutation::Under(w.0.clone()));
    assert_eq!(t.describe(&call_req), "rename_symbol Engine::start -> launch");
    let o = call(&t, "rename_symbol", json!({"symbol": "Engine::start", "new_name": "launch"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("Renamed Engine::start to `launch`:"), "{}", o.content);
    assert!(o.content.contains("--- a/src/main.rs\n+++ b/src/main.rs") && o.content.contains("+    e.launch();"), "{}", o.content);
    assert!(w.get("src/engine.rs").contains("pub fn launch(&self)"));
    assert!(w.get("src/main.rs").contains("e.launch();"));
    // The index saw it: the new name resolves at once.
    let o = call(&t, "definition", json!({"symbol": "Engine::launch"}));
    assert!(o.content.contains("src/engine.rs:4"), "{}", o.content);
    let o = call(&t, "rename_symbol", json!({"symbol": "Engine::launch", "new_name": "1bad"}));
    assert!(o.is_error && o.content.contains("not an identifier"));
    assert!(call(&t, "rename_symbol", json!({"symbol": "x"})).is_error);

    // Without a server a rename is refused, not guessed.
    let plain = BuiltinTools::new(Some(w.0.clone()));
    let o = call(&plain, "rename_symbol", json!({"symbol": "Engine::launch", "new_name": "go"}));
    assert!(o.is_error && o.content.contains("needs a language server"), "{}", o.content);
    assert!(w.get("src/engine.rs").contains("pub fn launch(&self)"));
    // ... while lookups fall back to the index.
    let o = call(&plain, "references", json!({"symbol": "Engine::launch"}));
    assert!(o.content.contains("(tags index)") && o.content.contains("same-named symbols are not told apart"), "{}", o.content);
}

struct Sandbox(PathBuf);

impl SandboxView for Sandbox {
    fn root(&self) -> &Path {
        &self.0
    }
    fn allows_write(&self, path: &Path) -> bool {
        path.starts_with(&self.0)
    }
    fn classify_shell(&self, _command: &str, _tainted: bool) -> Consequence {
        Consequence::Free
    }
    fn classify_code(&self, _code: &str) -> Consequence {
        Consequence::Free
    }
}

#[test]
fn rename_asks_where_writes_are_not_free() {
    let req = ToolCall { id: "r".into(), name: "rename_symbol".into(), args: json!({"symbol": "a", "new_name": "b"}) };
    // A drive root frees nothing.
    let broad = BuiltinTools::new(Some(PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })));
    assert!(matches!(broad.consequence(&req), Consequence::Ask { .. }));
    let w = project();
    let sb = BuiltinTools::new_sandbox(w.0.clone());
    assert_eq!(sb.consequence_in_sandbox(&req, &Exposure::default(), &Sandbox(w.0.clone())), Some(Consequence::Free));
    // Reads are free; a path outside the workspace taints.
    let t = BuiltinTools::new(Some(w.0.clone()));
    let outside = ToolCall { id: "d".into(), name: "diagnostics".into(), args: json!({"path": "C:/Windows/x.rs"}) };
    assert_eq!(t.consequence(&outside), Consequence::Free);
    assert!(t.taints(&outside));
    assert!(!t.taints(&ToolCall { id: "d".into(), name: "symbols".into(), args: json!({"query": "a"}) }));
}

/// The loop's order around one call (`run_tools.rs::run_one`): mutation, `pre_mutation`, the
/// call, `after_call`.
fn run_through_middleware(t: &BuiltinTools, mw: &[Arc<dyn ToolMiddleware>], name: &str, args: Value) -> ToolOutcome {
    let call = ToolCall { id: format!("{name}-1"), name: name.into(), args };
    let cx = CallCtx::detached(ctx(t.workspace().unwrap()));
    let mutation = t.mutation(&call, &cx.tool);
    if mutation != Mutation::None {
        for m in mw {
            if let Err(o) = m.pre_mutation(&call, &cx, &mutation) {
                return o;
            }
        }
    }
    let mut out = t.call_scoped(&call, &cx);
    for m in mw {
        m.after_call(&call, &cx, &mut out);
    }
    out
}

#[test]
fn parity_t24_an_edit_that_adds_an_error_gets_the_new_diagnostic_appended() {
    let w = project();
    w.put("src/lib.rs", "pub fn a() {}\n// ERROR: already broken\n");
    let (_l, t) = host(&w, FakeConfig::default());
    let mw = t.middleware();
    assert_eq!(mw.iter().filter(|m| m.name() == "code-diagnostics").count(), 1);

    let o = run_through_middleware(
        &t,
        &mw,
        "write",
        json!({"path": "src/lib.rs", "content": "pub fn a() {}\n// ERROR: already broken\npub fn b() {}\n// ERROR: type mismatch here\n"}),
    );
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("<new-diagnostics>"), "{}", o.content);
    assert!(o.content.contains("src/lib.rs: 1 new error (2 errors, 0 warnings in the file now)"), "{}", o.content);
    assert!(o.content.contains("4:4 error[F1]: type mismatch here"), "{}", o.content);
    assert!(!o.content.contains("already broken"), "old errors are not new: {}", o.content);

    // A clean edit appends nothing; the edited file is re-tagged.
    let o = run_through_middleware(&t, &mw, "write", json!({"path": "src/lib.rs", "content": "pub fn c() {}\n"}));
    assert!(!o.content.contains("<new-diagnostics>"), "{}", o.content);
    let o = call(&t, "symbols", json!({"query": "c", "kind": "function"}));
    assert!(o.content.contains("src/lib.rs:1"), "{}", o.content);

    // A failed edit and a non-code file are left alone.
    let o = run_through_middleware(&t, &mw, "edit", json!({"path": "src/lib.rs", "replacing": "nope", "content": "x"}));
    assert!(o.is_error && !o.content.contains("<new-diagnostics>"));
    let o = run_through_middleware(&t, &mw, "write", json!({"path": "notes.md", "content": "ERROR: not code"}));
    assert!(!o.content.contains("<new-diagnostics>"));
}

#[test]
fn the_diagnostics_tool_checks_a_file_and_lists_what_servers_know() {
    let w = project();
    w.put("src/bad.rs", "fn x() {}\n// ERROR: broken\n// WARN: odd\n");
    let (_l, t) = host(&w, FakeConfig::default());
    let o = call(&t, "diagnostics", json!({}));
    assert!(o.is_error && o.content.contains("no language server is running"), "{}", o.content);
    let o = call(&t, "diagnostics", json!({"path": "src/bad.rs"}));
    assert!(o.content.starts_with("src/bad.rs: 1 error, 1 warning"), "{}", o.content);
    assert!(o.content.contains("2:4 error[F1]: broken") && o.content.contains("3:4 warning[F1]: odd"), "{}", o.content);
    assert_eq!(o.summary, "1 error, 1 warning");
    let o = call(&t, "diagnostics", json!({"path": "src/bad.rs", "severity": "error"}));
    assert!(!o.content.contains("warning[F1]"), "{}", o.content);
    let o = call(&t, "diagnostics", json!({}));
    assert!(o.content.contains("src/bad.rs"), "{}", o.content);
    let o = call(&t, "diagnostics", json!({"path": "src/main.rs"}));
    assert_eq!(o.content, "No errors or warnings in src/main.rs.");
}

#[test]
fn parity_t27_the_repo_map_is_budgeted_for_the_model() {
    let w = project();
    for i in 0..30 {
        w.put(&format!("src/m{i}.rs"), &format!("pub fn helper_{i}(e: &Engine) -> u32 {{ e.start() }}\n"));
    }
    let t = BuiltinTools::new(Some(w.0.clone()));
    let map = repo_map(&t, &w.0, Vec::new(), Vec::new(), 32_768);
    assert!(map.starts_with("src/engine.rs\n"), "{map}");
    assert!(marlowe_code::repomap::estimate_tokens(&map) <= 1_200);
    let small = repo_map(&t, &w.0, vec![w.0.join("src/m7.rs")], Vec::new(), 32_768);
    assert!(small.contains("helper_7"), "{small}");
}
