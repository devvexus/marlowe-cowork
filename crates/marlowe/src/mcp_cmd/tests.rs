use std::sync::Arc;

use marlowe_mcp::fake::{FakeHttp, HttpOpts};
use marlowe_mcp::http::{DefaultHttp, HttpBackend, HttpRequest};
use marlowe_mcp::oauth::MemoryTokens;
use marlowe_mcp::util::Url;

use super::*;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-mcpcmd-{tag}-{}-{}", std::process::id(), marlowe_mcp::util::random_token(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cx(tag: &str) -> Cx {
    Cx { home: dir(&format!("{tag}-home")), workspace: Some(dir(&format!("{tag}-ws"))), tokens: Arc::new(MemoryTokens::default()), open: Box::new(|_| Ok(())) }
}

fn sh(cx: &Cx, line: &str) -> (i32, String) {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let mut out = Vec::new();
    let code = execute(cx, &args, &mut out).unwrap_or_else(|e| {
        out.extend_from_slice(format!("error: {e}").as_bytes());
        1
    });
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn parity_x15_add_list_get_remove_round_trip_through_the_files() {
    let cx = cx("rt");
    let (c, o) = sh(&cx, "add -s project files -e TOKEN=secret123 -- node server.js --root .");
    assert_eq!((c, o.as_str()), (0, "Added stdio MCP server files to the project config\n"));
    // The file is Claude Code's `.mcp.json`.
    let text = std::fs::read_to_string(cx.workspace.as_ref().unwrap().join(".mcp.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["mcpServers"]["files"], json!({"command": "node", "args": ["server.js", "--root", "."], "env": {"TOKEN": "secret123"}}));

    let (_, o) = sh(&cx, "list");
    assert!(o.contains("files: node server.js --root .  [project stdio]  ok"), "{o}");
    let (_, o) = sh(&cx, "get files");
    assert!(o.contains("Command: node") && o.contains("Env: TOKEN=***") && !o.contains("secret123"), "values never print: {o}");

    let (c, o) = sh(&cx, "remove files");
    assert_eq!((c, o.as_str()), (0, "Removed MCP server files from the project config\n"));
    assert!(sh(&cx, "list").1.starts_with("No MCP servers configured"));
    assert!(sh(&cx, "remove files").1.contains("no MCP server named"));
}

#[test]
fn parity_x15_scopes_land_in_their_own_files_and_local_is_the_default() {
    let cx = cx("scopes");
    let ws = cx.workspace.clone().unwrap();
    sh(&cx, "add a -- cmd-a");
    sh(&cx, "add -s user b -- cmd-b");
    sh(&cx, "add -s project c -- cmd-c");
    assert!(std::fs::read_to_string(ws.join(".marlowe").join("mcp.local.json")).unwrap().contains("cmd-a"));
    assert!(std::fs::read_to_string(cx.home.join("mcp.json")).unwrap().contains("cmd-b"));
    assert!(std::fs::read_to_string(ws.join(".mcp.json")).unwrap().contains("cmd-c"));
    let (_, o) = sh(&cx, "list");
    for (n, s) in [("a", "local"), ("b", "user"), ("c", "project")] {
        assert!(o.contains(&format!("{n}: cmd-{n}  [{s} stdio]")), "{o}");
    }
    assert_eq!(sh(&cx, "add -s nowhere x -- y").0, 1, "an unknown scope is an error");
}

#[test]
fn parity_x15_http_servers_take_headers_and_json_entries_take_anything_parse_entry_accepts() {
    let cx = cx("http");
    let (c, o) = sh(&cx, "add -t http -s user web https://example.com/mcp -H Authorization:Bearer-${WEB_TOKEN}");
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("http MCP server web (example.com)"), "{o}");
    let (_, o) = sh(&cx, "get web");
    assert!(o.contains("URL: https://example.com/mcp") && o.contains("Header: Authorization: Bearer-${WEB_TOKEN}"), "variables stay readable: {o}");

    let (c, o) = sh(&cx, r#"add-json -s user j {"command":"py","args":["-m","srv"],"consequence":"inert"}"#);
    assert_eq!(c, 0, "{o}");
    assert!(sh(&cx, "get j").1.contains("Consequence: inert"));
    let (c, o) = sh(&cx, r#"add-json -s user bad {"url":3}"#);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("error"), "{o}");
}

#[test]
fn parity_x15_a_workspace_entry_needs_trust_and_a_changed_one_asks_again() {
    let cx = cx("trust");
    let ws = cx.workspace.clone().unwrap();
    // Written by someone else (a cloned repository), not by `mcp add`.
    std::fs::write(ws.join(".mcp.json"), r#"{"mcpServers":{"evil":{"command":"calc"}}}"#).unwrap();
    assert!(sh(&cx, "list").1.contains("needs trust"));
    assert!(sh(&cx, "trust evil").1.contains("Trusted evil"));
    assert!(sh(&cx, "list").1.contains("[project stdio]  ok"));
    std::fs::write(ws.join(".mcp.json"), r#"{"mcpServers":{"evil":{"command":"calc","args":["--changed"]}}}"#).unwrap();
    assert!(sh(&cx, "list").1.contains("needs trust"), "a changed entry is a new decision");
    assert!(sh(&cx, "untrust evil").1.contains("No longer trusting"));
    assert!(sh(&cx, "trust nope").1.contains("no MCP server named"));
}

#[test]
fn parity_x15_list_check_connects_to_a_server_and_counts_its_tools() {
    let server = FakeHttp::start(HttpOpts::default());
    let cx = cx("check");
    assert_eq!(sh(&cx, &format!("add -s user live {}", server.url())).0, 0);
    let (c, o) = sh(&cx, "list --check");
    assert_eq!(c, 0);
    assert!(o.contains("live:") && o.contains("connected (") && o.contains("tools)"), "{o}");
}

fn browser(url: &str) -> Result<(), String> {
    let resp = DefaultHttp.send(&HttpRequest::get(&Url::parse(url)?), Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let target = Url::parse(resp.header("location").ok_or("no redirect")?)?;
    std::thread::spawn(move || {
        let _ = DefaultHttp.send(&HttpRequest::get(&target), Duration::from_secs(5));
    });
    Ok(())
}

#[test]
fn parity_x13_auth_signs_in_through_the_loopback_flow_and_logout_forgets() {
    let server = FakeHttp::start(HttpOpts { oauth: true, token_ttl: 3600, ..Default::default() });
    let mut cx = cx("auth");
    cx.open = Box::new(browser);
    assert_eq!(sh(&cx, &format!("add -s user web {}", server.url())).0, 0);
    assert!(sh(&cx, "get web").1.contains("OAuth: not signed in"));
    let (c, o) = sh(&cx, "auth web");
    assert_eq!((c, o.as_str()), (0, "Signed in to web.\n"));
    assert!(sh(&cx, "get web").1.contains("OAuth: signed in"));
    // The token is what lets the server answer.
    assert!(sh(&cx, "list --check").1.contains("connected"));
    assert_eq!(sh(&cx, "logout web").1, "Signed out of web.\n");
    assert!(sh(&cx, "get web").1.contains("not signed in"));
}

#[test]
fn auth_for_a_stdio_server_says_why_it_cannot() {
    let cx = cx("auth-stdio");
    sh(&cx, "add -s user s -- cmd");
    let (c, o) = sh(&cx, "auth s");
    assert_eq!(c, 1);
    assert!(o.contains("only http servers use OAuth"), "{o}");
}

#[test]
fn the_parser_takes_equals_forms_and_stops_at_double_dash() {
    let a: Vec<String> = ["--scope=user", "-e", "A=1", "name", "--", "cmd", "-x", "--scope", "z"].map(String::from).to_vec();
    let p = parse(&a, &["-s", "--scope", "-e"]).unwrap();
    assert_eq!(p.flag(&["--scope"]), Some("user"));
    assert_eq!(p.all(&["-e"]), ["A=1"]);
    assert_eq!(p.positional, ["name"]);
    assert_eq!(p.tail, ["cmd", "-x", "--scope", "z"]);
    assert!(parse(&["-s".to_string()], &["-s"]).is_err());
}
