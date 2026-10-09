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
    assert_eq!(names, ["analyst", "engineer", "researcher", "reviewer", "scout"]);
    for card in roles.cards() {
        assert_eq!(card.source, RoleSource::Builtin);
        assert_eq!(card.slot, Slot::Crew(card.name.clone()), "{}", card.name);
        assert_eq!(card.slot_chain(), vec![Slot::Crew(card.name.clone()), Slot::Conductor]);
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
    assert!(scout.allows("arxiv") && scout.allows("paper") && !scout.allows("write"));
    assert_eq!(roles.get("researcher").unwrap().sandbox, SandboxKind::Scratch);
    assert_eq!(roles.infos().len(), 5);
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
    assert_eq!(roles.cards().count(), 6);
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
    assert_eq!(schema["required"], json!(["role", "goal"]));
    assert_eq!(schema["properties"]["role"]["enum"], json!(["analyst", "engineer", "researcher", "reviewer", "scout"]));
    assert_eq!(schema["properties"]["playbook"]["enum"], json!(["deep_research", "build"]));
    assert_eq!(schema["additionalProperties"], json!(false));

    let a = DispatchArgs::parse(&json!({"role": "researcher", "goal": "LoRA rank", "playbook": "deep_research", "budget": {"usd": 2.0}}))
        .unwrap();
    assert_eq!(a.playbook, Some(Playbook::DeepResearch));
    assert_eq!(a.budget, Some(CrewBudget { usd: Some(2.0), ..Default::default() }));
    assert!(DispatchArgs::parse(&json!({"role": "scout", "goal": "  "})).unwrap_err().contains("goal"));
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
