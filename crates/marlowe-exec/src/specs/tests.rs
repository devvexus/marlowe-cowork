//! The table against the code it replaced: `legacy` is the pre-table `match` dispatch of
//! consequence, taint, egress and description, copied verbatim from `lib.rs` as of M4.5. Every
//! call in the matrix must get the same answer from both, under every exposure and host shape.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use marlowe_agent::{Consequence, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolHost};
use marlowe_model::CancelToken;
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::table;
use crate::{first_line, more_lines, research, research_input, str_arg, BuiltinTools};

/// The M4.5 dispatch, verbatim but for `self` -> `t`.
mod legacy {
    use super::*;

    pub fn consequence(t: &BuiltinTools, call: &ToolCallReq) -> Consequence {
        let args = &call.args;
        match call.name.as_str() {
            "write" | "edit" => match str_arg(args, "path") {
                Some(p) if t.inside_root(p) => Consequence::Free,
                Some(p) => Consequence::Ask {
                    summary: format!("{} {} (outside the workspace)", call.name, first_line(p, 100)),
                },
                None => Consequence::Free,
            },
            "bash" | "powershell" => {
                let command = str_arg(args, "command").unwrap_or("");
                Consequence::Ask {
                    summary: format!("{}: {}{}", call.name, first_line(command, 100), more_lines(command)),
                }
            }
            "python" => Consequence::Ask {
                summary: match (str_arg(args, "code"), str_arg(args, "path")) {
                    (Some(code), _) => format!("python: {}{}", first_line(code, 100), more_lines(code)),
                    (None, Some(path)) => format!("python {}", first_line(path, 100)),
                    _ => "python".to_string(),
                },
            },
            _ => Consequence::Free,
        }
    }

    pub fn consequence_exposed(t: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if exposure.tainted {
            let args = &call.args;
            match call.name.as_str() {
                "web_fetch" => {
                    if let Some(host) = egress_host(t, call).filter(|h| !exposure.approved_hosts.contains(h)) {
                        return Consequence::Ask {
                            summary: format!("web_fetch {host}: a new host, after untrusted content"),
                        };
                    }
                }
                "arxiv" => {
                    return Consequence::Ask {
                        summary: format!(
                            "arxiv: {} (after untrusted content)",
                            first_line(str_arg(args, "query").unwrap_or(""), 100)
                        ),
                    }
                }
                "web_search" => {
                    return Consequence::Ask {
                        summary: format!(
                            "web_search: {} (after untrusted content)",
                            first_line(str_arg(args, "query").unwrap_or(""), 100)
                        ),
                    }
                }
                _ => {}
            }
        }
        consequence(t, call)
    }

    pub fn taints(t: &BuiltinTools, call: &ToolCallReq) -> bool {
        match call.name.as_str() {
            "web_fetch" | "web_search" | "arxiv" => true,
            "read" | "glob" | "grep" => !t.trusted_read(str_arg(&call.args, "path").unwrap_or(".")),
            _ => false,
        }
    }

    pub fn egress_host(_t: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
        if call.name != "web_fetch" {
            return None;
        }
        // A paper read (an arXiv id or URL, openreview, doi.org, or `part`) goes through the paper reader.
        let paperish = str_arg(&call.args, "part").is_some()
            || research::arxiv_id(research_input(&call.args)).is_some()
            || marlowe_net::Target::parse(research_input(&call.args)).is_ok_and(|t| t.host == "doi.org" || t.host.ends_with("openreview.net"));
        if paperish {
            return research::paper_host(research_input(&call.args));
        }
        let url = str_arg(&call.args, "url")?;
        marlowe_net::Target::parse(url).ok().map(|t| t.host)
    }

    pub fn describe(call: &ToolCallReq) -> String {
        let args = &call.args;
        let key = match call.name.as_str() {
            "read" | "write" | "edit" => "path",
            "glob" => "pattern",
            "grep" => "pattern",
            "bash" | "powershell" => "command",
            "python" => "code",
            "web_fetch" => "url",
            "web_search" | "arxiv" => "query",
            _ => "",
        };
        let detail = str_arg(args, key).or_else(|| str_arg(args, "path")).map(|s| first_line(s, 60)).unwrap_or_default();
        if detail.is_empty() {
            call.name.clone()
        } else {
            format!("{} {}", call.name, detail)
        }
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-exec-table-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn hosts() -> Vec<(&'static str, BuiltinTools)> {
    let root = temp_root("root");
    let reads = temp_root("reads");
    vec![
        ("no root", BuiltinTools::new(None)),
        ("root", BuiltinTools::new(Some(root.clone()))),
        ("root + read root", BuiltinTools::new(Some(root)).with_read_root(reads)),
        ("broad root", BuiltinTools::new(Some(PathBuf::from("C:\\")))),
    ]
}

fn calls() -> Vec<ToolCallReq> {
    let root = temp_root("root");
    let inside = root.join("src").join("lib.rs").display().to_string();
    let read_root = temp_root("reads").join("notes.md").display().to_string();
    let args: Vec<(&str, Value)> = vec![
        ("read", json!({"path": "src/main.rs"})),
        ("read", json!({"path": inside})),
        ("read", json!({"path": read_root})),
        ("read", json!({"path": "C:\\Windows\\win.ini", "range": "1-5"})),
        ("read", json!({})),
        ("write", json!({"path": "out.txt", "content": "x"})),
        ("write", json!({"path": "..\\..\\escape.txt", "content": "x"})),
        ("write", json!({"content": "no path"})),
        ("edit", json!({"path": inside, "replacing": "a", "content": "b"})),
        ("edit", json!({"path": "C:\\elsewhere\\f.rs", "replacing": "a", "content": "b"})),
        ("glob", json!({"pattern": "**/*.rs"})),
        ("glob", json!({"pattern": "*.toml", "path": "C:\\Program Files"})),
        ("grep", json!({"pattern": "fn main", "path": read_root})),
        ("grep", json!({"pattern": "TODO"})),
        ("bash", json!({"command": "cargo test"})),
        ("bash", json!({"command": "echo one\necho two\necho three"})),
        ("bash", json!({})),
        ("powershell", json!({"command": "Get-ChildItem", "cwd": "src"})),
        ("python", json!({"code": "print(1)\nprint(2)"})),
        ("python", json!({"path": "scripts/run.py"})),
        ("python", json!({})),
        ("web_fetch", json!({"url": "https://example.com/page"})),
        ("web_fetch", json!({"url": "https://approved.example.org/x"})),
        ("web_fetch", json!({"url": "not a url"})),
        ("web_search", json!({"query": "rust job objects\nsecond line"})),
        ("arxiv", json!({"query": "ti:transformers"})),
        ("web_fetch", json!({"url": "2301.01234"})),
        ("web_fetch", json!({"url": "https://openreview.net/pdf?id=abc"})),
        ("web_fetch", json!({"url": "https://approved.example.org/p.pdf", "part": "references"})),
        ("unknown_tool", json!({"path": "x", "text": "y"})),
        ("unknown_tool", json!({})),
    ];
    args.into_iter()
        .enumerate()
        .map(|(i, (name, args))| ToolCallReq { id: format!("c{i}"), name: name.into(), args })
        .collect()
}

fn exposures() -> Vec<Exposure> {
    let approved: HashSet<String> = ["approved.example.org".to_string(), "arxiv.org".to_string()].into();
    vec![
        Exposure::default(),
        Exposure { tainted: true, approved_hosts: HashSet::new() },
        Exposure { tainted: true, approved_hosts: approved },
    ]
}

#[test]
fn every_tool_answers_as_the_old_dispatch_did() {
    let mut checked = 0;
    for (shape, host) in hosts() {
        for call in calls() {
            let what = format!("{shape}: {} {}", call.name, call.args);
            assert_eq!(host.consequence(&call), legacy::consequence(&host, &call), "consequence, {what}");
            for exposure in exposures() {
                assert_eq!(
                    host.consequence_exposed(&call, &exposure),
                    legacy::consequence_exposed(&host, &call, &exposure),
                    "consequence_exposed ({exposure:?}), {what}"
                );
            }
            assert_eq!(host.taints(&call), legacy::taints(&host, &call), "taints, {what}");
            assert_eq!(host.egress_host(&call), legacy::egress_host(&host, &call), "egress, {what}");
            assert_eq!(host.describe(&call), legacy::describe(&call), "describe, {what}");
            checked += 1;
        }
    }
    assert!(checked > 100);
}

#[test]
fn every_tool_is_registered_once_in_its_group_in_the_old_order() {
    // The git (track J) and verify (track D) groups pin their own lists in their own tests, and so
    // do the wave-2 groups (workflow, tasks, notify).
    let names: Vec<&str> = table()
        .defs
        .iter()
        .filter(|d| {
            !matches!(
                d.manifest.group,
                marlowe_agent::ToolGroup::Git
                    | marlowe_agent::ToolGroup::Verify
                    | marlowe_agent::ToolGroup::Workflow
                    | marlowe_agent::ToolGroup::Tasks
                    | marlowe_agent::ToolGroup::Notify
            )
        })
        .map(|d| d.name())
        .collect();
    assert_eq!(
        names,
        [
            "read", "write", "edit", "multi_edit", "edit_lines", "notebook_edit", "glob", "grep", "bash", "powershell",
            "python", "web_fetch", "arxiv", "web_search", "symbols",
            "definition", "references", "hover", "diagnostics", "rename_symbol", "todo",
            "ask_user", "skill", "agent",
            "send_message", "enter_plan", "exit_plan", "enter_worktree", "exit_worktree"
        ]
    );
    let git: Vec<&str> = table().defs.iter().filter(|d| d.manifest.group == marlowe_agent::ToolGroup::Git).map(|d| d.name()).collect();
    assert_eq!(git, ["git", "gh"]);
    let verify: Vec<&str> =
        table().defs.iter().filter(|d| d.manifest.group == marlowe_agent::ToolGroup::Verify).map(|d| d.name()).collect();
    assert_eq!(verify, ["project", "service", "http_check", "browser"]);
    for d in &table().defs {
        let schema = &d.spec.input_schema;
        assert_eq!(schema["type"], json!("object"), "{} schema is an object", d.name());
        for (key, p) in schema["properties"].as_object().into_iter().flatten() {
            assert!(!matches!(p.get("type").and_then(Value::as_str), Some("object")) || key == "args" || key == "budget", "{}.{key} is flat", d.name());
        }
    }
    let host = BuiltinTools::new(None);
    assert_eq!(host.manifest("write").unwrap().group, marlowe_agent::ToolGroup::Files);
    assert!(host.manifest("write").unwrap().effects.writes);
    assert!(!host.manifest("read").unwrap().effects.writes);
    assert!(host.manifest("bash").unwrap().effects.runs);
    assert!(host.manifest("nope").is_none());
}

struct FakeSandbox {
    root: PathBuf,
}

impl SandboxView for FakeSandbox {
    fn root(&self) -> &Path {
        &self.root
    }
    fn allows_write(&self, path: &Path) -> bool {
        !path.to_string_lossy().contains("..")
    }
    fn classify_shell(&self, command: &str, tainted: bool) -> Consequence {
        if tainted || command.contains("push") {
            Consequence::Ask { summary: format!("pushes: {command}") }
        } else {
            Consequence::Free
        }
    }
    fn classify_code(&self, code: &str) -> Consequence {
        if code.contains("socket") {
            Consequence::Ask { summary: "opens a socket".into() }
        } else {
            Consequence::Free
        }
    }
}

/// The crew's `CrewTools::consequence_exposed` match, as of M4.5, expressed against the table.
#[test]
fn crew_consequences_match_the_crew_host() {
    let sb = FakeSandbox { root: temp_root("sandbox") };
    std::fs::write(sb.root.join("net.py"), "import socket").unwrap();
    let host = BuiltinTools::new_sandbox(sb.root.clone());
    let call = |name: &str, args: Value| ToolCallReq { id: "c".into(), name: name.into(), args };
    let clean = Exposure::default();
    let tainted = Exposure { tainted: true, approved_hosts: HashSet::new() };

    let w = call("write", json!({"path": "a.txt", "content": ""}));
    assert_eq!(host.consequence_in_sandbox(&w, &clean, &sb), Some(Consequence::Free));
    let w = call("edit", json!({"path": "../x.txt"}));
    assert_eq!(
        host.consequence_in_sandbox(&w, &clean, &sb),
        Some(Consequence::Ask { summary: "edit ../x.txt (outside the sandbox)".into() })
    );
    assert_eq!(host.consequence_in_sandbox(&call("write", json!({})), &clean, &sb), Some(Consequence::Free));
    let b = call("bash", json!({"command": "git push"}));
    assert_eq!(
        host.consequence_in_sandbox(&b, &clean, &sb),
        Some(Consequence::Ask { summary: "bash: pushes: git push".into() })
    );
    assert_eq!(host.consequence_in_sandbox(&call("powershell", json!({"command": "ls"})), &clean, &sb), Some(Consequence::Free));
    assert_eq!(host.consequence_in_sandbox(&call("python", json!({"code": "print(1)"})), &clean, &sb), Some(Consequence::Free));
    assert_eq!(
        host.consequence_in_sandbox(&call("python", json!({"path": "net.py"})), &clean, &sb),
        Some(Consequence::Ask { summary: "python: opens a socket".into() })
    );
    // Tainted python falls back to the plain consequence, which asks.
    assert!(matches!(
        host.consequence_in_sandbox(&call("python", json!({"code": "print(1)"})), &tainted, &sb),
        Some(Consequence::Ask { .. } | Consequence::AskEvery { .. })
    ));
    // Tools without a crew consequence defer to the plain one.
    assert_eq!(host.consequence_in_sandbox(&call("read", json!({"path": "a"})), &clean, &sb), None);
    assert_eq!(host.consequence_in_sandbox(&call("web_fetch", json!({"url": "https://x.org"})), &tainted, &sb), None);
}

#[test]
fn mutations_name_what_a_call_will_change() {
    let root = temp_root("mut");
    let host = BuiltinTools::new(Some(root.clone()));
    let ctx = ToolCtx { session: SessionId("s".into()), turn: 0, workspace: None, cancel: CancelToken::new(), tainted: false };
    let call = |name: &str, args: Value| ToolCallReq { id: "c".into(), name: name.into(), args };
    assert_eq!(host.mutation(&call("read", json!({"path": "a"})), &ctx), Mutation::None);
    assert_eq!(
        host.mutation(&call("write", json!({"path": "a.txt", "content": ""})), &ctx),
        Mutation::Paths(vec![crate::paths::resolve(Some(&root), "a.txt")])
    );
    assert_eq!(host.mutation(&call("edit", json!({})), &ctx), Mutation::None);
    assert_eq!(host.mutation(&call("bash", json!({"command": "ls"})), &ctx), Mutation::Under(root.clone()));
    assert_eq!(
        host.mutation(&call("bash", json!({"command": "ls", "cwd": "sub"})), &ctx),
        Mutation::Under(crate::paths::resolve(Some(&root), "sub"))
    );
}

#[derive(Default)]
struct Counter(std::sync::atomic::AtomicU32);

#[test]
fn host_state_is_one_value_per_type() {
    let host = BuiltinTools::new(None).with_state(String::from("configured"));
    host.state::<Counter>().0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    host.state::<Counter>().0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(host.state::<Counter>().0.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(host.state::<String>().as_str(), "configured");
    assert_eq!(host.workspace_port().kind(), "local");
}

#[test]
fn exposure_fixes_the_tool_set_and_refuses_the_rest() {
    use marlowe_agent::{ExposureProfile, ToolGroup};
    let names = |h: &BuiltinTools| h.specs().into_iter().map(|s| s.name).collect::<Vec<_>>();
    let everything = BuiltinTools::new(None);
    // A local model in a workspace with nothing detected gets the core groups: today that is
    // every registered tool, so the default and the local set agree.
    let local = marlowe_model::ModelInfo::bare(marlowe_model::ProviderKind::Ollama, "m");
    let frontier = marlowe_model::ModelInfo::bare(marlowe_model::ProviderKind::OpenRouter, "m");
    let without_verify = |h: &BuiltinTools| {
        names(h).into_iter().filter(|n| !["project", "service", "http_check", "browser"].contains(&n.as_str())).collect::<Vec<_>>()
    };
    // Nothing hides behind a search: a local model on the assumed 32k window lists the same tools.
    assert_eq!(names(&BuiltinTools::new(None).with_model_exposure(&local)), without_verify(&everything));
    assert_eq!(BuiltinTools::new(None).with_model_exposure(&local).exposure().0, ExposureProfile::Local);
    assert_eq!(BuiltinTools::new(None).with_model_exposure(&frontier).exposure().1, ToolGroup::ALL.as_slice());

    let files_only = BuiltinTools::new(None).with_exposure(ExposureProfile::Local, vec![ToolGroup::Files]);
    // `edit_lines` is off unless a host asks for it (`with_edit_lines`).
    assert_eq!(names(&files_only), vec!["read", "write", "edit", "multi_edit", "notebook_edit"]);
    let ctx = ToolCtx { session: SessionId("s".into()), turn: 0, workspace: None, cancel: CancelToken::new(), tainted: false };
    let out = files_only.call(&ToolCallReq { id: "c".into(), name: "bash".into(), args: json!({"command": "echo hi"}) }, &ctx);
    assert!(out.is_error && out.content.contains("not available in this session"), "{}", out.content);
    // The files group's one middleware is its FileState guard (read-before-edit, stale refusals).
    assert_eq!(files_only.middleware().len(), 1);
    assert!(files_only.persona_blocks().is_empty(), "the files group has no persona block");
}

/// Track J's finding 4b: a host that offers the shells but not the git group still refuses a
/// force push typed into bash.
#[test]
fn parity_g7_the_shell_guard_holds_without_the_git_group() {
    use marlowe_agent::{CallCtx, CallFlow, ExposureProfile, ToolGroup};
    let host = BuiltinTools::new(None).with_exposure(ExposureProfile::Local, vec![ToolGroup::Files, ToolGroup::Shell]);
    let mw = host.middleware();
    assert!(mw.iter().any(|m| m.name() == "git-guard"), "{:?}", mw.iter().map(|m| m.name().to_string()).collect::<Vec<_>>());
    let ctx = ToolCtx { session: SessionId("s".into()), turn: 0, workspace: None, cancel: CancelToken::new(), tainted: false };
    let cx = CallCtx::detached(ctx);
    let mut push = ToolCallReq { id: "c".into(), name: "bash".into(), args: json!({"command": "git push --force origin main"}) };
    let refused = mw.iter().any(|m| matches!(m.before_approval(&mut push, &cx), CallFlow::Done(_)));
    assert!(refused, "a force push through bash is refused without the git group");
    let mut status = ToolCallReq { id: "c".into(), name: "bash".into(), args: json!({"command": "git status"}) };
    assert!(mw.iter().all(|m| matches!(m.before_approval(&mut status, &cx), CallFlow::Continue)));
}

#[test]
fn every_group_file_registers_its_hooks() {
    let setup = super::HostSetup { home: Path::new("C:/home"), workspace: None };
    let host = super::configure(BuiltinTools::new(None), &setup);
    // Configuring adds the git group (track J), the agent group (track F), the workflow group
    // (track W), the tasks and notify groups (track N) and, where Edge is installed, the browser (D).
    let rest = |h: &BuiltinTools| {
        h.specs()
            .iter()
            .filter(|s| !matches!(s.name.as_str(), "git" | "gh" | "ci_status" | "browser"))
            .filter(|s| {
                h.manifest(&s.name).is_some_and(|m| {
                    !matches!(
                        m.group,
                        marlowe_agent::ToolGroup::Agent
                            | marlowe_agent::ToolGroup::Workflow
                            | marlowe_agent::ToolGroup::Tasks
                            | marlowe_agent::ToolGroup::Notify
                    )
                })
            })
            .count()
    };
    assert_eq!(rest(&host), rest(&BuiltinTools::new(None)), "the other configures change nothing here");
    assert!(host.specs().iter().any(|s| s.name == "git"), "the git group (track J) is live once configured");
    for g in super::GROUPS {
        assert_eq!(super::group(g.group).map(|r| r.group), Some(g.group));
        // Files: the FileState guard (A). Shell: job notices (B). Git: the git guard (J). Workflow: the .star validator. Code
        // adds post-edit diagnostics only with a local workspace (C).
        let expected = match g.group {
            marlowe_agent::ToolGroup::Git | marlowe_agent::ToolGroup::Shell | marlowe_agent::ToolGroup::Files | marlowe_agent::ToolGroup::Workflow => 1,
            _ => 0,
        };
        assert_eq!((g.middleware)(&host).len(), expected, "{:?}", g.group);
        assert_eq!(
            (g.persona_block)().is_some(),
            matches!(
                g.group,
                marlowe_agent::ToolGroup::Git | marlowe_agent::ToolGroup::Verify | marlowe_agent::ToolGroup::Agent
            ),
            "{:?}: a block needs its capability live first",
            g.group
        );
        // Workflows are offered to a local model in any workspace (W).
        assert_eq!((g.detect)(None), g.group == marlowe_agent::ToolGroup::Workflow, "{:?}", g.group);
    }
    assert!(super::group(marlowe_agent::ToolGroup::Memory).is_none(), "memory tools are the daemon's");
}

#[test]
fn the_group_override_names_known_groups_only() {
    use marlowe_agent::ToolGroup;
    assert_eq!(super::parse_group_list("shell").unwrap(), vec![ToolGroup::Shell]);
    assert_eq!(super::parse_group_list(" files , shell,shell ").unwrap(), vec![ToolGroup::Files, ToolGroup::Shell]);
    assert!(super::parse_group_list("shell,nonsense").unwrap_err().contains("nonsense"));
    assert!(super::parse_group_list(" , ").is_err());
}
