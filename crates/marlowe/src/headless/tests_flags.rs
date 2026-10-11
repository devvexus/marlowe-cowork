//! Each `marlowe -p` flag against a recording fake daemon: what request it becomes (design 2.7 H4,
//! "one test per flag").

use marlowe_proto::{Event, PermissionMode, Request, SessionId, SessionSummary, SettingsScope, Slot};
use serde_json::json;

use super::args::Args;
use super::testkit::*;

fn run_fake(fake: &Fake, argv: &[&str]) -> Outcome {
    let home = temp_dir("flags-home");
    let ws = home.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut v: Vec<String> = vec!["-p".into(), "--workspace".into(), ws.display().to_string()];
    v.extend(argv.iter().map(|s| s.to_string()));
    run_args(Args::parse(&v).unwrap_or_else(|e| panic!("{v:?}: {e}")), &home, &ws, None, fake.connector())
}

fn options(fake: &Fake) -> marlowe_proto::HeadlessOptions {
    fake.ask().expect("an Ask was sent").2.expect("with options")
}

#[test]
fn parity_h4_allowed_and_disallowed_tools_reach_the_ask() {
    let fake = Fake::start();
    let o = run_fake(&fake, &["--allowed-tools", "Bash(git diff:*),Read", "--disallowed-tools", "Bash(rm:*)", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let opts = options(&fake);
    assert_eq!(opts.allowed_tools, ["Bash(git diff:*)", "Read"]);
    assert_eq!(opts.disallowed_tools, ["Bash(rm:*)"]);
}

#[test]
fn parity_h4_max_steps_and_budget_and_system_prompt_reach_the_ask() {
    let fake = Fake::start();
    let o = run_fake(&fake, &["--max-steps", "12", "--max-budget-usd", "0.5", "--append-system-prompt", "be terse", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let opts = options(&fake);
    assert_eq!(opts.max_steps, Some(12));
    assert_eq!(opts.max_budget_usd, Some(0.5));
    assert_eq!(opts.append_system_prompt.as_deref(), Some("be terse"));
    // A bare run sends nothing but the print marker (subagents then run in the foreground).
    let fake = Fake::start();
    run_fake(&fake, &["hi"]);
    assert_eq!(fake.ask().unwrap().2, Some(marlowe_proto::HeadlessOptions { print: true, ..Default::default() }));
}

#[test]
fn parity_h4_permission_mode_is_set_on_the_session_and_in_the_options() {
    let fake = Fake::start();
    let o = run_fake(&fake, &["--permission-mode", "acceptEdits", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(fake.find(|r| match r {
        Request::SetMode { session, mode } => Some((session.clone(), *mode)),
        _ => None,
    }), Some((SessionId("fake-session".into()), PermissionMode::AcceptEdits)));
    assert_eq!(options(&fake).permission_mode, Some(PermissionMode::AcceptEdits));
    // The daemon track that owns modes is not there yet: said once on stderr, the run goes on.
    assert!(o.err.text().contains("--permission-mode is not supported by this daemon yet"), "{}", o.err.text());
}

#[test]
fn parity_h4_a_daemon_that_supports_set_mode_is_not_complained_at() {
    let fake = Fake::with(|r| match r {
        Request::SetMode { session, mode } => vec![Event::ModeChanged { session: session.clone(), mode: *mode }],
        other => default_reply(other),
    });
    let o = run_fake(&fake, &["--permission-mode", "plan", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.err.text(), "");
}

#[test]
fn parity_h4_model_adds_it_when_missing_and_prefers_a_session_switch() {
    // Not in the library: added, then switched; with no per-session switch yet, the Conductor slot.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--model", "ollama/other", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let reqs = fake.requests();
    assert!(reqs.iter().any(|r| matches!(r, Request::AddModel { model_id, .. } if model_id == "other")));
    assert!(reqs.iter().any(|r| matches!(r, Request::SwitchModel { model, .. } if model.to_string() == "ollama/other")));
    assert!(reqs.iter().any(|r| matches!(r, Request::SetSlot { slot: Slot::Conductor, model: Some(m), .. } if m.to_string() == "ollama/other")), "{reqs:?}");

    // In the library already, and the daemon switches per session: no slot change.
    let fake = Fake::with(|r| match r {
        Request::SwitchModel { .. } => Vec::new(),
        other => default_reply(other),
    });
    let o = run_fake(&fake, &["--model", "ollama/scripted", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let reqs = fake.requests();
    assert!(!reqs.iter().any(|r| matches!(r, Request::AddModel { .. } | Request::SetSlot { .. })), "{reqs:?}");
    assert!(reqs.iter().any(|r| matches!(r, Request::SwitchModel { .. })));

    // A malformed ref is refused before anything is sent.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--model", "nonsense", "hi"]);
    assert_eq!(o.code, 1);
    assert!(fake.ask().is_none());
}

#[test]
fn parity_h4_add_dir_sends_the_canonical_path() {
    let fake = Fake::start();
    let extra = temp_dir("flags-extra");
    let o = run_fake(&fake, &["--add-dir", extra.to_str().unwrap(), "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let sent = fake.find(|r| match r {
        Request::AddDir { path, .. } => Some(path.clone()),
        _ => None,
    });
    assert_eq!(sent.map(|p| p.to_ascii_lowercase()), Some(extra.canonicalize().map(crate::strip_verbatim).unwrap().display().to_string().to_ascii_lowercase()));
    // A directory that is not there is an error before the turn.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--add-dir", "C:/no/such/dir/at/all", "hi"]);
    assert_eq!(o.code, 1);
    assert!(fake.ask().is_none());
}

#[test]
fn parity_h4_mcp_config_adds_each_server_for_the_session_only() {
    let fake = Fake::start();
    let cfg = json!({"mcpServers": {"fs": {"command": "node", "args": ["server.js"]}, "web": {"type": "http", "url": "http://127.0.0.1:9/mcp"}}}).to_string();
    let o = run_fake(&fake, &["--mcp-config", &cfg, "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let added: Vec<(String, SettingsScope)> = fake
        .requests()
        .iter()
        .filter_map(|r| match r {
            Request::McpAdd { name, scope, .. } => Some((name.clone(), *scope)),
            _ => None,
        })
        .collect();
    assert_eq!(added, [("fs".to_string(), SettingsScope::Session), ("web".to_string(), SettingsScope::Session)]);
    // A bad entry stops the run before it starts.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--mcp-config", r#"{"mcpServers":{"bad name!":{"command":"x"}}}"#, "hi"]);
    assert_eq!(o.code, 1, "{}", o.err.text());
    assert!(fake.requests().is_empty(), "nothing was sent");
}

#[test]
fn parity_h4_strict_mcp_config_means_a_process_of_its_own() {
    let a = Args::parse(&["-p".into(), "--strict-mcp-config".into(), "--mcp-config".into(), "{}".into(), "hi".into()]).unwrap();
    assert!(a.strict_mcp && a.standalone);
}

#[test]
fn parity_h4_settings_become_rules_a_mode_and_session_settings() {
    let fake = Fake::start();
    let s = json!({"permissions": {"allow": ["Read"], "deny": ["WebFetch"], "defaultMode": "acceptEdits"}, "network": {"allow_hosts": ["x.com"]}}).to_string();
    let o = run_fake(&fake, &["--settings", &s, "--allowed-tools", "Glob", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let opts = options(&fake);
    assert_eq!(opts.allowed_tools, ["Glob", "Read"], "flags first, then the file");
    assert_eq!(opts.disallowed_tools, ["WebFetch"]);
    assert_eq!(opts.permission_mode, Some(PermissionMode::AcceptEdits));
    let set = fake.find(|r| match r {
        Request::SetSetting { scope, key, value, .. } => Some((*scope, key.clone(), value.clone())),
        _ => None,
    });
    assert_eq!(set, Some((SettingsScope::Session, "network".into(), json!({"allow_hosts": ["x.com"]}))));
    // A settings value that cannot be read stops the run.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--settings", "nope.json", "hi"]);
    assert_eq!(o.code, 1);
    assert!(fake.requests().is_empty());
}

#[test]
fn parity_h4_continue_picks_the_newest_session_of_the_workspace_and_resume_names_one() {
    let home = temp_dir("flags-resume");
    let ws = home.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let canon = crate::strip_verbatim(ws.canonicalize().unwrap()).display().to_string();
    let c2 = canon.clone();
    let fake = Fake::with(move |r| match r {
        Request::ListSessions { .. } => vec![Event::SessionList {
            sessions: vec![
                SessionSummary { id: "older".into(), title: "a".into(), workspace: Some("C:/other".into()), created_ms: 1, updated_ms: 50, turns: 1 },
                SessionSummary { id: "mine-old".into(), title: "b".into(), workspace: Some(c2.to_ascii_uppercase()), created_ms: 1, updated_ms: 10, turns: 1 },
                SessionSummary { id: "mine-new".into(), title: "c".into(), workspace: Some(c2.clone()), created_ms: 1, updated_ms: 20, turns: 1 },
            ],
        }],
        other => default_reply(other),
    });
    let run = |argv: &[&str]| {
        let mut v: Vec<String> = vec!["-p".into(), "--workspace".into(), ws.display().to_string()];
        v.extend(argv.iter().map(|s| s.to_string()));
        run_args(Args::parse(&v).unwrap(), &home, &ws, None, fake.connector())
    };
    let o = run(&["-c", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(fake.ask().unwrap().0, Some(SessionId("mine-new".into())));
    assert!(fake.requests().iter().any(|r| matches!(r, Request::OpenSession { session } if session.0 == "mine-new")));
    assert!(!fake.requests().iter().any(|r| matches!(r, Request::NewSession { .. })), "no new session when resuming");

    let fake2 = Fake::with(move |r| match r {
        Request::ListSessions { .. } => vec![Event::SessionList {
            sessions: vec![SessionSummary { id: "abcdef123456".into(), title: "x".into(), workspace: None, created_ms: 1, updated_ms: 1, turns: 1 }],
        }],
        other => default_reply(other),
    });
    let mut v: Vec<String> = vec!["-p".into(), "-r".into(), "abcdef12".into(), "hi".into()];
    v.insert(1, "--workspace".into());
    v.insert(2, ws.display().to_string());
    let o = run_args(Args::parse(&v).unwrap(), &home, &ws, None, fake2.connector());
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(fake2.ask().unwrap().0, Some(SessionId("abcdef123456".into())), "a unique prefix names it");
}

#[test]
fn parity_h4_workspace_and_worktree_choose_where_the_session_lives() {
    let fake = Fake::start();
    let o = run_fake(&fake, &["hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let ws = fake.find(|r| match r {
        Request::NewSession { workspace } => workspace.clone(),
        _ => None,
    });
    assert!(ws.is_some_and(|w| w.to_ascii_lowercase().ends_with("ws")), "the --workspace directory, canonical");

    // -w: a worktree of the repository.
    let repo = temp_dir("flags-wt");
    for args in [vec!["init", "-q", "-b", "main"], vec!["config", "user.email", "t@example.com"], vec!["config", "user.name", "t"], vec!["config", "commit.gpgsign", "false"]] {
        super::worktree::git(&repo, &args).unwrap();
    }
    std::fs::write(repo.join("a.txt"), "x").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "first"]] {
        super::worktree::git(&repo, &args).unwrap();
    }
    let fake = Fake::start();
    let home = temp_dir("flags-wt-home");
    let a = Args::parse(&["-p".into(), "--workspace".into(), repo.display().to_string(), "-w".into(), "feat".into(), "hi".into()]).unwrap();
    let o = run_args(a, &home, &repo, None, fake.connector());
    assert_eq!(o.code, 0, "{}", o.err.text());
    let ws = fake
        .find(|r| match r {
            Request::NewSession { workspace } => workspace.clone(),
            _ => None,
        })
        .unwrap();
    assert!(ws.replace('\\', "/").to_ascii_lowercase().ends_with(".marlowe/worktrees/feat"), "{ws}");
    assert!(std::path::Path::new(&ws).join("a.txt").is_file());
    // A directory that is not a repository cannot host one.
    let plain = temp_dir("flags-wt-plain");
    let a = Args::parse(&["-p".into(), "--workspace".into(), plain.display().to_string(), "-w".into(), "feat".into(), "hi".into()]).unwrap();
    let fake = Fake::start();
    let o = run_args(a, &home, &plain, None, fake.connector());
    assert_eq!(o.code, 1);
    assert!(o.err.text().contains("git repository"), "{}", o.err.text());
}

#[test]
fn parity_h4_output_and_input_format_flags_and_the_partial_flag() {
    // Covered end to end in tests.rs; here the combinations the parser refuses.
    for bad in [
        vec!["-p", "--input-format", "stream-json"],
        vec!["-p", "--include-partial-messages"],
        vec!["-p", "--output-format", "xml"],
    ] {
        assert!(Args::parse(&bad.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_err(), "{bad:?}");
    }
}

#[test]
fn parity_h7_the_key_comes_from_the_store_or_the_environment_and_is_never_reported() {
    use marlowe_brains::{CredentialStore, MemoryStore};
    use marlowe_model::ProviderKind::{Anthropic, OpenRouter};
    let none = |_: &str| None;
    let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-ant-secret".to_string());
    let store = MemoryStore::new();
    assert_eq!(super::driver::key_source(Anthropic, &store, &none), "none", "a clean container has no key");
    assert_eq!(super::driver::key_source(Anthropic, &store, &env), "env", "the environment variable is the fallback");
    assert_eq!(super::driver::key_source(OpenRouter, &store, &env), "none", "another provider's variable does not count");
    store.set(Anthropic, "stored").unwrap();
    assert_eq!(super::driver::key_source(Anthropic, &store, &env), "credential_manager", "a stored credential wins");
    // The init line names a source, not a value.
    let src = super::driver::key_source(Anthropic, &store, &env);
    assert!(!src.contains("sk-ant") && !src.contains("stored"));
}

#[test]
fn parity_h7_the_provider_keys_are_scrubbed_from_every_child() {
    for name in ["ANTHROPIC_API_KEY", "OPENROUTER_API_KEY", "OPENAI_API_KEY"] {
        assert!(marlowe_os::child::is_scrubbed(name), "{name}");
    }
}

// ---------------------------------------------------------------- options only a daemon can keep

fn run_fake_unapplied(fake: &Fake, argv: &[&str]) -> Outcome {
    let home = temp_dir("flags-unapplied");
    let ws = home.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut v: Vec<String> = vec!["-p".into(), "--workspace".into(), ws.display().to_string()];
    v.extend(argv.iter().map(|s| s.to_string()));
    run_args_unapplied(Args::parse(&v).unwrap_or_else(|e| panic!("{v:?}: {e}")), &home, &ws, None, fake.connector())
}

#[test]
fn parity_h4_disallowed_tools_and_an_appended_prompt_refuse_to_run_when_the_daemon_cannot_apply_them() {
    // Today's daemon does not read HeadlessOptions: a denied write inside the workspace would
    // still happen, and the appended text would never reach the model. Exit 1, nothing sent.
    for (argv, named) in [
        (vec!["--disallowed-tools", "Write", "hi"], "--disallowed-tools"),
        (vec!["--append-system-prompt", "be terse", "hi"], "--append-system-prompt"),
        (vec!["--settings", r#"{"permissions":{"deny":["Write"]}}"#, "hi"], "--disallowed-tools"),
    ] {
        let fake = Fake::start();
        let o = run_fake_unapplied(&fake, &argv);
        assert_eq!(o.code, 1, "{argv:?}: {}", o.err.text());
        assert!(o.err.text().contains(named) && o.err.text().contains("cannot be honoured"), "{argv:?}: {}", o.err.text());
        assert!(fake.requests().is_empty(), "{argv:?}: nothing was sent: {:?}", fake.requests());
    }
    // The flags this process keeps by itself are unaffected.
    let fake = Fake::start();
    let o = run_fake_unapplied(&fake, &["--allowed-tools", "Read", "--max-steps", "3", "--max-budget-usd", "1", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
}

fn free_write_rig(name: &str) -> Rig {
    Rig::with_tools(name, std::sync::Arc::new(|| std::sync::Arc::new(FreeWrite) as std::sync::Arc<dyn marlowe_agent::ToolHost>))
}

/// What the flags do now that the conductor applies them (`DAEMON_APPLIES_OPTIONS`).
#[test]
fn parity_h4_a_disallowed_write_inside_the_workspace_does_not_happen() {
    let rig = free_write_rig("h4-disallowed-write");
    let target = rig.ws.join("out.txt");
    rig.script.push(vec![Ev::Call("w1", "write", json!({"path": target.display().to_string(), "content": "x"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("done"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--disallowed-tools", "write", "write the file"]);
    assert!(!target.exists(), "the denied write ran: {}", o.out.text());
    assert!(o.denials() >= 1 || !rig.script.requests()[0].tools.iter().any(|t| t.name == "write"), "denied or never offered");
}

#[test]
fn parity_h4_an_appended_system_prompt_reaches_the_model() {
    let rig = Rig::new("h4-append");
    rig.script.push(vec![Ev::Text("ok"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--append-system-prompt", "ANSWER-IN-HAIKU", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let system: String = rig.script.requests()[0].system.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n");
    assert!(system.contains("ANSWER-IN-HAIKU"), "{system}");
}

// ---------------------------------------------------------------- plan mode, run servers, ignored settings

#[test]
fn parity_h4_plan_mode_is_an_error_when_the_daemon_cannot_hold_it() {
    // The default fake answers SetMode with "not available yet". For plan that is fatal: writes
    // inside the workspace never ask, so a plan run on such a daemon would edit files.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--permission-mode", "plan", "hi"]);
    assert_eq!(o.code, 1, "{}", o.err.text());
    assert!(o.err.text().contains("plan") && o.err.text().contains("cannot hold"), "{}", o.err.text());
    assert!(fake.ask().is_none(), "no turn was started");
    // The same from a settings file.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--settings", r#"{"permissions":{"defaultMode":"plan"}}"#, "hi"]);
    assert_eq!(o.code, 1);
    assert!(fake.ask().is_none());
}

#[test]
fn parity_h4_mcp_config_servers_are_taken_out_again_on_every_road() {
    let cfg = json!({"mcpServers": {"fs": {"command": "node", "args": ["server.js"]}}}).to_string();
    let removed = |fake: &Fake| {
        fake.requests().iter().any(|r| matches!(r, Request::McpRemove { name, scope: SettingsScope::Session, .. } if name == "fs"))
    };
    // A finished run.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--mcp-config", &cfg, "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert!(removed(&fake), "{:?}", fake.requests());
    // A run that fails after the servers were added.
    let fake = Fake::start();
    let o = run_fake(&fake, &["--mcp-config", &cfg, "--add-dir", "C:/no/such/dir/at/all", "hi"]);
    assert_eq!(o.code, 1);
    assert!(removed(&fake), "{:?}", fake.requests());
}

#[test]
fn parity_h4_a_session_mcp_entry_cannot_widen_the_environment_and_the_run_says_so() {
    let fake = Fake::start();
    let cfg = json!({"mcpServers": {"fs": {"command": "node", "env_allow": ["OPENROUTER_API_KEY"]}}}).to_string();
    let o = run_fake(&fake, &["--mcp-config", &cfg, "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert!(o.err.text().contains("env_allow is honoured in the user scope only"), "{}", o.err.text());
}
