//! Seam tests: role cards, the dispatch declaration, run records, prose helpers.

use std::path::PathBuf;

use marlowe_model::Effort;
use marlowe_proto::{Confidence, CrewBudget, CrewRunStatus, Playbook, SandboxKind, SessionId, Slot};
use serde_json::json;

use crate::dispatch::dispatch_spec;
use crate::playbook::{confidence_line, detect_test_command, list_items, markdown_links, slug};
use crate::sandbox::{branch_name, no_push_git_env, NO_PUSH_URL};
use crate::*;

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-crew-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn every_builtin_card_parses_with_its_intended_shape() {
    let roles = Roles::builtin();
    let names: Vec<&str> = roles.cards().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "analyst",
            "compressor",
            "engineer",
            "integrator",
            "lead",
            "researcher",
            "review-correctness",
            "review-performance",
            "review-security",
            "reviewer",
            "scout",
            "verifier",
            "workflow"
        ]
    );
    for card in roles.cards() {
        assert_eq!(card.source, RoleSource::Builtin);
        // The compression role is cheap work on the Quick slot; every other card has its own.
        let slot = if card.name == "compressor" { Slot::Quick } else { Slot::Crew(card.name.clone()) };
        assert_eq!(card.slot, slot, "{}", card.name);
        assert_eq!(card.slot_chain(), vec![slot, Slot::Conductor]);
        assert!(!card.description.is_empty() && card.instructions.len() > 400, "{}", card.name);
        assert!(card.budget.tokens.is_some() && card.budget.steps.is_some() && card.budget.usd.is_some(), "{}", card.name);
        assert!(!card.tools.iter().any(|t| t == DISPATCH_TOOL), "{} must not dispatch", card.name);
        assert!(!card.instructions.contains("Matthew"), "role cards never name the operator");
    }
    let eng = roles.get("Engineer").unwrap();
    assert_eq!(eng.sandbox, SandboxKind::Worktree);
    assert_eq!(eng.effort, Effort::High);
    assert!(eng.allows("bash") && eng.allows("edit") && !eng.allows("web_fetch"));
    let scout = roles.get("scout").unwrap();
    assert_eq!(scout.sandbox, SandboxKind::None);
    assert_eq!(scout.budget, CrewBudget { tokens: Some(150_000), steps: Some(25), usd: Some(0.5) });
    assert!(scout.allows("arxiv") && scout.allows("web_fetch") && !scout.allows("write"));
    assert_eq!(roles.get("researcher").unwrap().sandbox, SandboxKind::Scratch);
    assert_eq!(roles.infos().len(), 13);
    for name in ["compressor", "verifier"] {
        let card = roles.get(name).unwrap();
        assert!(card.tools.is_empty() && card.sandbox == SandboxKind::None, "{name} reads only what it is given");
    }
}

#[test]
fn a_user_card_overrides_by_name_and_a_bad_one_is_reported_not_fatal() {
    let home = temp_home("roles");
    let dir = home.join("roles");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("scout.md"),
        "---\nname: scout\ndescription: Faster scout.\nslot: quick\neffort: off\ntools: [web_search]\n---\nBe quick.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("statistician.md"),
        "---\nname: statistician\ndescription: Checks the maths.\ntools: [python, read]  # local only\nbudget: {usd: $1.25, steps: 30}\nsandbox: scratch\n---\nCheck every test statistic.\n",
    )
    .unwrap();
    std::fs::write(dir.join("broken.md"), "name: nope\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

    let roles = Roles::load(&home);
    let scout = roles.get("scout").unwrap();
    assert_eq!(scout.slot, Slot::Quick);
    assert_eq!(scout.slot_chain(), vec![Slot::Quick, Slot::Conductor]);
    assert_eq!(scout.effort, Effort::Off);
    assert_eq!(scout.tools, ["web_search"]);
    assert_eq!(scout.instructions, "Be quick.");
    assert!(matches!(&scout.source, RoleSource::User(p) if p.ends_with("scout.md")));
    assert!(scout.info().user_file.is_some());

    let stat = roles.get("statistician").unwrap();
    assert_eq!(stat.tools, ["python", "read"]);
    assert_eq!(stat.budget, CrewBudget { tokens: None, steps: Some(30), usd: Some(1.25) });
    assert_eq!(stat.effort, Effort::Medium);
    assert_eq!(roles.cards().count(), 14, "the 13 built-in cards and the user's statistician");
    assert_eq!(roles.problems().len(), 1);
    assert_eq!(roles.problems()[0].1, RoleError::NoFrontmatter);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn malformed_cards_say_what_is_wrong() {
    let card = |front: &str| parse_role(&format!("---\n{front}\n---\nDo it.\n"), RoleSource::Builtin);
    assert_eq!(card("description: d").unwrap_err(), RoleError::Missing("name"));
    assert_eq!(card("name: x").unwrap_err(), RoleError::Missing("description"));
    assert_eq!(card("name: x\ndescription: d\ncolour: red").unwrap_err(), RoleError::UnknownField("colour".into()));
    assert!(matches!(card("name: a b\ndescription: d").unwrap_err(), RoleError::BadField { field: "name", .. }));
    assert!(matches!(card("name: x\ndescription: d\neffort: huge").unwrap_err(), RoleError::BadField { field: "effort", .. }));
    assert!(matches!(card("name: x\ndescription: d\nsandbox: vm").unwrap_err(), RoleError::BadField { field: "sandbox", .. }));
    assert!(matches!(card("name: x\ndescription: d\nbudget: {cats: 3}").unwrap_err(), RoleError::BadField { field: "budget", .. }));
    assert!(matches!(card("name: x\ndescription: d\ntools: [a, b").unwrap_err(), RoleError::BadField { field: "tools", .. }));
    assert_eq!(parse_role("---\nname: x\ndescription: d\n---\n\n", RoleSource::Builtin).unwrap_err(), RoleError::NoInstructions);
    assert_eq!(parse_role("---\nname: x\n", RoleSource::Builtin).unwrap_err(), RoleError::NoFrontmatter);
    let ok = card("name: X-1\ndescription: Uses # freely\nsandbox: worktree").unwrap();
    assert_eq!((ok.name.as_str(), ok.description.as_str(), ok.sandbox), ("x-1", "Uses # freely", SandboxKind::Worktree));
    assert!(!ok.allows(DISPATCH_TOOL));
}

#[test]
fn the_dispatch_declaration_lists_roles_and_its_arguments_parse() {
    let roles = Roles::builtin().infos();
    let spec = dispatch_spec(&roles);
    assert_eq!(spec.name, "dispatch");
    let schema = &spec.input_schema;
    assert_eq!(schema["required"], json!(["role", "prompt"]));
    assert_eq!(schema["properties"]["role"]["enum"], json!([
            "analyst",
            "compressor",
            "engineer",
            "integrator",
            "lead",
            "researcher",
            "review-correctness",
            "review-performance",
            "review-security",
            "reviewer",
            "scout",
            "verifier"
        ]));
    assert_eq!(schema["properties"]["playbook"]["enum"], json!(["deep_research", "build"]));

    let a = DispatchArgs::parse(&json!({"role": "researcher", "prompt": "LoRA rank", "cwd": "C:/w", "playbook": "deep_research", "budget": {"usd": 2.0}}))
        .unwrap();
    assert_eq!(a.playbook, Some(Playbook::DeepResearch));
    assert_eq!(a.budget, Some(CrewBudget { usd: Some(2.0), ..Default::default() }));
    assert_eq!((a.goal.as_str(), a.workspace.as_deref()), ("LoRA rank", Some("C:/w")), "prompt and cwd are the schema's names for goal and workspace");
    assert!(DispatchArgs::parse(&json!({"role": "scout", "goal": "  "})).unwrap_err().contains("prompt"));
    assert!(DispatchArgs::parse(&json!({"goal": "x"})).is_err());
    assert!(DispatchArgs::parse(&json!({"role": "scout", "goal": "x", "playbook": "dance"})).is_err());

    let receipt = DispatchReceipt {
        run_id: RunId("ab".into()),
        role: "scout".into(),
        playbook: None,
        estimate: Estimate { minutes: 5, usd: Some(0.1) },
    };
    assert_eq!(
        serde_json::to_string(&receipt).unwrap(),
        r#"{"run_id":"ab","role":"scout","estimate":{"minutes":5,"usd":0.1}}"#
    );
}

#[test]
fn a_new_run_is_queued_with_the_role_budget_overlaid() {
    let roles = Roles::builtin();
    let role = roles.get("engineer").unwrap();
    let goal = "Add a --json flag to `marlowe status` that prints the StatusReport as one JSON object, with a test";
    let run = Run::new(
        Some(SessionId("s1".into())),
        role,
        goal,
        Some(Playbook::Build),
        Some(PathBuf::from("C:/w")),
        CrewBudget { usd: Some(1.0), ..Default::default() },
    );
    assert_eq!(run.status, CrewRunStatus::Queued);
    assert_eq!(run.budget, CrewBudget { tokens: role.budget.tokens, steps: role.budget.steps, usd: Some(1.0) });
    assert!(run.title.chars().count() <= 61 && run.title.ends_with('…'), "{}", run.title);
    let info = run.info();
    assert_eq!((info.id, info.role.as_str(), info.goal.as_str()), (run.id.clone(), "engineer", goal));
    assert_eq!(info.workspace.as_deref(), Some("C:/w"));
    assert_eq!(title_from_goal("  short goal\nmore detail "), "short goal");
    assert_eq!(branch_name(&RunId("abc".into())), "crew/abc");

    let back: Run = serde_json::from_str(&serde_json::to_string(&run).unwrap()).unwrap();
    assert_eq!(back, run, "the run record round-trips for the store");
    RunStore::open_in_memory().expect("schema applies");
}

#[test]
fn crew_shells_get_a_git_config_that_cannot_push() {
    let env = no_push_git_env(&["origin".into(), "fork".into()]);
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let n: usize = get("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
    assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
    let pairs: Vec<(&str, &str)> =
        (0..n).map(|i| (get(&format!("GIT_CONFIG_KEY_{i}")).unwrap(), get(&format!("GIT_CONFIG_VALUE_{i}")).unwrap())).collect();
    assert!(pairs.contains(&("remote.origin.pushurl", NO_PUSH_URL)));
    assert!(pairs.contains(&("remote.fork.pushurl", NO_PUSH_URL)));
    assert!(pairs.contains(&("credential.helper", "")));
    assert!(pairs.iter().any(|(k, v)| k.ends_with(".pushInsteadOf") && *v == "https://"));
    assert_eq!(env.len(), 2 + 2 * n);
}

#[test]
fn prose_helpers_read_lists_links_and_confidence() {
    let plan = "Sub-questions:\n1. What ranks were tested?\n2) Which tasks?\n- Is there a replication?\n\nNot a list line.\n10. Tenth.";
    assert_eq!(list_items(plan), ["What ranks were tested?", "Which tasks?", "Is there a replication?", "Tenth."]);

    let text = "See [Hu et al. 2021](https://arxiv.org/abs/2106.09685) and [notes](docs/lora.md); \
                again [LoRA](https://arxiv.org/abs/2106.09685). Not [a link] (x). [bad](has space)";
    let links = markdown_links(text);
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].title, "Hu et al. 2021");
    assert_eq!(links[1].url_or_path, "docs/lora.md");

    assert_eq!(confidence_line("report\nConfidence: medium\n"), Some(Confidence::Medium));
    assert_eq!(confidence_line("**Confidence: High.**"), Some(Confidence::High));
    assert_eq!(confidence_line("Confidence: low\nlater\nconfidence: high"), Some(Confidence::High));
    assert_eq!(confidence_line("I am confident."), None);

    assert_eq!(slug("What does the literature say about LoRA rank?"), "what-does-the-literature-say-about-lora-rank");
    assert_eq!(slug("¿?"), "research");
    assert!(slug(&"word ".repeat(40)).len() <= 60);
}

#[test]
fn test_commands_are_detected_from_the_manifest() {
    let dir = temp_home("detect");
    assert_eq!(detect_test_command(&dir), None);
    std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"tsc"}}"#).unwrap();
    assert_eq!(detect_test_command(&dir), None, "no test script");
    std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"vitest run"}}"#).unwrap();
    assert_eq!(detect_test_command(&dir).as_deref(), Some("npm test"));
    std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    assert_eq!(detect_test_command(&dir).as_deref(), Some("cargo test"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- seam-crew: tool host, policy

mod seam {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use marlowe_agent::{
        CallCtx, Consequence, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolHost, ToolMiddleware, ToolOutcome,
    };
    use marlowe_model::{CancelToken, ToolSpec};
    use marlowe_proto::{SandboxKind, SessionId};
    use serde_json::{json, Value};

    use crate::shell_policy::{classify_sandbox_shell, Dialect, Verdict};
    use crate::{CrewTools, Sandbox, ShellVerdict};

    fn sandbox() -> Sandbox {
        let root = std::env::temp_dir().join("marlowe-seam-crew-sandbox");
        Sandbox { kind: SandboxKind::Worktree, root, origin: None, branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None }
    }

    fn req(name: &str, args: Value) -> ToolCallReq {
        ToolCallReq { id: "c".into(), name: name.into(), args }
    }

    fn ctx() -> ToolCtx {
        ToolCtx { session: SessionId("s".into()), turn: 0, workspace: None, cancel: CancelToken::new(), tainted: false }
    }

    /// An inner host with a table-style crew consequence for `mark`, a plain one for the rest, and
    /// a `call_scoped` that answers differently from `call`.
    #[derive(Default)]
    struct Inner {
        calls: AtomicUsize,
        scoped: AtomicUsize,
    }

    impl ToolHost for Inner {
        fn specs(&self) -> Vec<ToolSpec> {
            ["mark", "other", "bash", "powershell", "hidden"]
                .map(|n| ToolSpec { name: n.into(), description: n.into(), input_schema: json!({"type": "object"}) })
                .to_vec()
        }

        fn call(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok("plain", "plain")
        }

        fn call_scoped(&self, _call: &ToolCallReq, _cx: &CallCtx) -> ToolOutcome {
            self.scoped.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok("scoped", "scoped")
        }

        fn consequence(&self, call: &ToolCallReq) -> Consequence {
            Consequence::Ask { summary: format!("plain {}", call.name) }
        }

        fn consequence_in_sandbox(
            &self,
            call: &ToolCallReq,
            _exposure: &Exposure,
            sandbox: &dyn SandboxView,
        ) -> Option<Consequence> {
            (call.name == "mark").then(|| Consequence::Ask { summary: format!("mark in {}", sandbox.root().display()) })
        }

        fn mutation(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> Mutation {
            Mutation::Under(Path::new("x").into())
        }

        fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
            vec![Arc::new(Nop)]
        }
    }

    struct Nop;
    impl ToolMiddleware for Nop {
        fn name(&self) -> &str {
            "nop"
        }
    }

    fn host(allow: &[&str]) -> (Arc<Inner>, CrewTools) {
        let inner = Arc::new(Inner::default());
        let tools = CrewTools::new(inner.clone(), allow.iter().map(|s| s.to_string()).collect(), sandbox());
        (inner, tools)
    }

    fn ran(inner: &Inner) -> (usize, usize) {
        (inner.scoped.load(Ordering::SeqCst), inner.calls.load(Ordering::SeqCst))
    }

    #[test]
    fn the_crew_host_forwards_the_loops_session_scope_and_what_belongs_to_the_tools() {
        let (inner, tools) = host(&["mark", "other"]);
        let cx = CallCtx::detached(ctx());
        assert_eq!(tools.call_scoped(&req("other", json!({})), &cx).content, "scoped");
        assert_eq!(tools.call(&req("other", json!({})), &ctx()).content, "plain");
        assert_eq!(ran(&inner), (1, 1));
        // Off the allowlist is refused on both paths, without reaching the inner host.
        for out in [tools.call_scoped(&req("hidden", json!({})), &cx), tools.call(&req("hidden", json!({})), &ctx())] {
            assert!(out.is_error && out.content.contains("not on this role's tool list"), "{}", out.content);
        }
        assert_eq!(ran(&inner), (1, 1));
        assert_eq!(tools.middleware().len(), 1);
        assert_eq!(tools.mutation(&req("other", json!({})), &ctx()), Mutation::Under("x".into()));
        let names: Vec<_> = tools.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["mark", "other"]);
    }

    #[test]
    fn crew_consequences_come_from_the_tool_table_and_fall_back_to_the_plain_one() {
        let (_, tools) = host(&["mark", "other", "bash", "powershell"]);
        let sb = sandbox();
        let clean = Exposure::default();
        let tainted = Exposure { tainted: true, ..Default::default() };
        let of = |name: &str, args: Value, e: &Exposure| tools.consequence_exposed(&req(name, args), e);
        assert_eq!(of("mark", json!({}), &clean), Consequence::Ask { summary: format!("mark in {}", sb.root.display()) });
        assert_eq!(of("other", json!({}), &clean), Consequence::Ask { summary: "plain other".into() });
        // Shells are decided by the sandbox policy in the tool's dialect, with the run's taint.
        assert_eq!(of("bash", json!({"command": "cargo test"}), &clean), Consequence::Free);
        let push = ["git", "push"].join(" ");
        assert!(matches!(of("bash", json!({"command": push}), &clean), Consequence::Ask { summary } if summary.starts_with("bash: ")));
        assert!(matches!(of("bash", json!({"command": "ls"}), &tainted), Consequence::Ask { summary } if summary.contains("untrusted")));
        assert!(matches!(of("powershell", json!({"command": "Get-ChildItem"}), &tainted), Consequence::Ask { .. }));
        assert_eq!(of("powershell", json!({"command": "Get-ChildItem"}), &clean), Consequence::Free);
    }

    #[test]
    fn hard_denies_are_refused_in_the_crew_host_whatever_the_run_has_read() {
        let (inner, tools) = host(&["bash", "powershell"]);
        let force = ["git", "push", "--force", "origin", "main"].join(" ");
        let skip_hooks = ["git", "commit", "--no-verify", "-m", "x"].join(" ");
        for (tool, cmd) in [("bash", &force), ("bash", &skip_hooks), ("powershell", &force)] {
            let out = tools.call_scoped(&req(tool, json!({"command": cmd})), &CallCtx::detached(ctx()));
            assert!(out.is_error && out.content.contains("refused"), "{tool} {cmd}: {}", out.content);
            let out = tools.call(&req(tool, json!({"command": cmd})), &ctx());
            assert!(out.is_error && out.content.contains("refused"), "{tool} {cmd}: {}", out.content);
        }
        assert_eq!(ran(&inner), (0, 0), "nothing ran");
        // Asked about, a denied command is a fixed approval, never a free one.
        let c = tools.consequence_exposed(&req("bash", json!({"command": force})), &Exposure::default());
        assert!(matches!(c, Consequence::AskEvery { .. }), "{c:?}");
        // The legacy verdict never reads a deny as free.
        assert!(matches!(sandbox().classify_shell(&force, false), ShellVerdict::Ask(_)));
    }

    #[test]
    fn the_sandbox_is_the_policys_view_and_the_re_export_is_the_permission_crates() {
        let sb = sandbox();
        let view: &dyn SandboxView = &sb;
        assert_eq!(view.root(), sb.root.as_path());
        assert!(view.allows_write(Path::new("src/main.rs")) && !view.allows_write(Path::new("../escape.txt")));
        assert_eq!(view.classify_shell("cargo build", false), Consequence::Free);
        assert!(view.classify_shell("rm -rf ../other", false).asks());
        assert_eq!(view.classify_code("print(1)"), Consequence::Free);
        // The same function the conductor's policy exports.
        let direct = classify_sandbox_shell(
            Dialect::Bash,
            &sb.root,
            marlowe_permission::shell::SandboxKind::Worktree,
            &|p| sb.allows_write(p),
            "cargo build",
            false,
        );
        assert_eq!(direct, Verdict::Free);
        assert_eq!(sb.shell_verdict(Dialect::Bash, "cargo build", false), direct);
    }
}

/// Each crew role and built-in subagent type, the tools it is given and what their specs cost
/// (characters over three, the estimate the budgets use). A role gets only what its job needs;
/// widening one changes this table on purpose.
fn role_table() -> String {
    let cost = |tools: &[String]| -> usize {
        tools
            .iter()
            .map(|t| {
                let s = marlowe_agent::compact_specs::spec(t);
                format!("{}{}{}", s.name, s.description, s.input_schema).chars().count().div_ceil(3)
            })
            .sum()
    };
    let mut rows = Vec::new();
    for card in Roles::builtin().cards() {
        rows.push(format!("role {:<19} {:>5}  {}", card.name, cost(&card.tools), card.tools.join(" ")));
    }
    for ty in marlowe_agent::subagent::builtin_types() {
        match &ty.tools {
            Some(t) => rows.push(format!("type {:<19} {:>5}  {}", ty.name, cost(t), t.join(" "))),
            None => rows.push(format!("type {:<19} {:>5}  (the caller's tools)", ty.name, "-")),
        }
    }
    rows.iter().map(|r| r.trim_end()).collect::<Vec<_>>().join("\n")
}

#[test]
fn the_per_role_tool_table_is_pinned() {
    let table = role_table();
    println!("{table}");
    assert_eq!(table, TABLE.trim());
    // A tool a role is given must exist, and a read-only job gets nothing that writes.
    for card in Roles::builtin().cards() {
        for t in &card.tools {
            marlowe_agent::compact_specs::spec(t);
        }
    }
    let roles = Roles::builtin();
    for name in ["scout", "reviewer", "review-correctness", "review-security", "review-performance"] {
        let c = roles.get(name).unwrap();
        assert!(!["write", "edit", "git"].iter().any(|t| c.allows(t)), "{name} does not change files");
    }
    assert!(!roles.get("researcher").unwrap().allows("edit") && roles.get("engineer").unwrap().allows("git"));
}

const TABLE: &str = "
role analyst               854  read glob grep write edit python bash powershell
role compressor              0
role engineer             1544  read write edit glob grep bash powershell python git project
role integrator           1456  read write edit glob grep bash powershell git project
role lead                 1006  read write edit glob grep bash powershell project
role researcher            698  read glob grep write web_search web_fetch arxiv
role review-correctness    605  read glob grep bash powershell
role review-performance    605  read glob grep bash powershell
role review-security       605  read glob grep bash powershell
role reviewer              888  read glob grep bash powershell web_search web_fetch arxiv
role scout                 637  web_search web_fetch arxiv read glob grep
role verifier                0
role workflow             1972  read write edit multi_edit notebook_edit glob grep bash powershell python web_fetch web_search git project
type explore               758  read glob grep symbols definition references hover diagnostics
type plan                  758  read glob grep symbols definition references hover diagnostics
type general                 -  (the caller's tools)
type fork                    -  (the caller's tools)
type guide                 354  read glob grep
type statusline            438  read glob grep set_statusline
type agent-creator         530  read glob grep create_agent
";
