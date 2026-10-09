//! **The claims the composition root makes about itself.** M2 C3, closing three gaps found by
//! auditing the session's own testability rather than by a failure.
//!
//! Each test here exists because something in the tree *asserted* a property in prose and nothing
//! checked it. Two of the three were claims I had already written down as true.
//!
//! | Claim | Where it was made | Was it checked |
//! |---|---|---|
//! | a batch reaches the innermost host **as a batch** | a comment in `build_tool_host`, **naming this test by a name that did not exist** | no |
//! | the exposed-tool budget refuses a third MCP tool | ADR-052 §5, and `STATE.md` said "unit-tested only" | **no — the claim was false** |
//! | a malformed `mcp.json` refuses to start | ADR-052 §6 | no |

use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_daemon::mcp::{McpFleet, McpTools};
use marlowe_daemon::recall::RecallTools;
use marlowe_daemon::skills::SkillTools;
use marlowe_loop::driver::{ToolBody, ToolHost, ToolOutcome};
use marlowe_loop::{BatchItem, CapabilityProfile};
use marlowe_permission::{Adjudication, ArgValue, Args};
use marlowe_tools::{ExposureError, Metric, ResultSummary, ToolId, MAX_EXPOSED_TOOLS};

// ─────────────────────────────────────────────────────────────────────────────────────────
// 1. The batch survives every wrapper
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Records whether it was handed one batch or a sequence of single calls.
#[derive(Default)]
struct Spy {
    batches: Arc<Mutex<Vec<usize>>>,
    singles: Arc<Mutex<usize>>,
}

impl ToolHost for Spy {
    fn executes(&self) -> Vec<ToolId> {
        vec![ToolId::new("web")]
    }
    fn execute(&mut self, _t: &ToolId, _a: &Args, _adj: &Adjudication) -> ToolOutcome {
        *self.singles.lock().unwrap() += 1;
        outcome("single")
    }
    fn execute_batch(&mut self, items: &[BatchItem<'_>]) -> Vec<ToolOutcome> {
        self.batches.lock().unwrap().push(items.len());
        items.iter().map(|_| outcome("batched")).collect()
    }
}

fn outcome(tag: &str) -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::State("ok")]),
        body: ToolBody::Inline(tag.to_string()),
        trust: TrustClass::UntrustedContent,
        failed: false,
        wall_ms: 0,
        preview: None,
    }
}

/// **The trap `RecallTools::execute_batch` documents, one and two layers further out.**
///
/// `ToolHost::execute_batch` has a serial default. That default is what makes the trait extension
/// safe for every implementor, and it is exactly what makes a *wrapper* dangerous: a wrapper that
/// does not override it turns the concurrent path into a `for` loop, and the concurrent path then
/// exists, is tested, is green, and never runs in the product.
///
/// `RecallTools` has its own test for that. C3 put **two more wrappers on top of it** —
/// `SkillTools` and `McpTools` — and a link that forwards to a link that does not is exactly as
/// broken as one that does not forward. So the assertion is on the **whole chain**, from the
/// outermost wrapper the daemon builds down to the host that would do the real fetching.
///
/// This test is named by a comment in `build_tool_host`. **That comment named it before it
/// existed** — found by auditing the session rather than by a failure, and it is the same family
/// as a guarded path that moved: a claim about a test is not a test.
#[test]
fn a_batch_reaches_the_innermost_host_through_both_wrappers() {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let singles = Arc::new(Mutex::new(0usize));
    let spy = Spy { batches: Arc::clone(&batches), singles: Arc::clone(&singles) };

    // The daemon's nesting, exactly: McpTools(SkillTools(RecallTools(<fetching host>))).
    let mut host = McpTools::new(
        SkillTools::new(
            RecallTools::new(spy, Arc::new(Mutex::new(marlowe_memory::BeliefStore::default()))),
            Arc::new(Mutex::new(marlowe_tools::skill::SkillRegistry::new())),
        ),
        Arc::new(Mutex::new(McpFleet::empty())),
    );

    let ids: Vec<ToolId> = (0..8).map(|_| ToolId::new("web")).collect();
    let args: Vec<Args> = (0..8)
        .map(|i| Args::new().with("url", ArgValue::Text(format!("https://ex{i}.example/"))))
        .collect();
    let adj = allowed("web");
    let items: Vec<BatchItem<'_>> = (0..8)
        .map(|i| BatchItem { tool: &ids[i], args: &args[i], adjudication: &adj })
        .collect();

    let out = host.execute_batch(&items);

    assert_eq!(out.len(), 8, "the loop attributes results positionally");
    assert_eq!(
        *batches.lock().unwrap(),
        vec![8],
        "the innermost host was not handed one batch of 8. A wrapper dropped to the serial \
         default, and the concurrent fetch is now dead in the product while its own tests \
         stay green"
    );
    assert_eq!(
        *singles.lock().unwrap(),
        0,
        "a wrapper fell back to per-call `execute`"
    );

    // **The control.** Without it, a chain that returned eight canned outcomes and never reached
    // the spy at all would satisfy every assertion above.
    for o in &out {
        let ToolBody::Inline(tag) = &o.body else { panic!("inline expected") };
        assert_eq!(tag, "batched", "the outcome did not come from the innermost host");
    }
}

/// Each wrapper still serves its OWN tool while forwarding the rest — the other half of the
/// property, and the one a "forward everything" wrapper would break.
#[test]
fn each_wrapper_still_answers_its_own_tool_inside_a_mixed_batch() {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let spy = Spy { batches: Arc::clone(&batches), singles: Arc::new(Mutex::new(0)) };

    let mut host = McpTools::new(
        SkillTools::new(
            RecallTools::new(spy, Arc::new(Mutex::new(marlowe_memory::BeliefStore::default()))),
            Arc::new(Mutex::new(marlowe_tools::skill::SkillRegistry::new())),
        ),
        Arc::new(Mutex::new(McpFleet::empty())),
    );

    let web = ToolId::new("web");
    let use_ = ToolId::new("use");
    let recall = ToolId::new("recall");
    let a = Args::new().with("url", ArgValue::Text("https://ex.example/".into()));
    let b = Args::new().with("query", ArgValue::Text("anything".into()));
    let adj = allowed("web");
    let items = vec![
        BatchItem { tool: &web, args: &a, adjudication: &adj },
        BatchItem { tool: &use_, args: &b, adjudication: &adj },
        BatchItem { tool: &recall, args: &b, adjudication: &adj },
    ];

    let out = host.execute_batch(&items);
    assert_eq!(out.len(), 3);

    // Only `web` was delegated: a batch of exactly one reached the bottom.
    assert_eq!(
        *batches.lock().unwrap(),
        vec![1],
        "the tools each wrapper serves must not be forwarded to the fetching host"
    );

    let body = |i: usize| match &out[i].body {
        ToolBody::Inline(s) => s.clone(),
        _ => panic!("inline expected"),
    };
    assert_eq!(body(0), "batched", "slot 0 is `web`, from the innermost host");
    assert!(
        body(1).contains("no skills are installed"),
        "slot 1 is `use`, answered by SkillTools: {}",
        body(1)
    );
    assert!(
        body(2).contains("no memories") || body(2).contains("Nothing has been remembered"),
        "slot 2 is `recall`, answered by RecallTools: {}",
        body(2)
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 2. The exposed-tool budget
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **ADR-052 §5, and `STATE.md` claimed this was already unit-tested. It was not.**
///
/// A tool past the budget must **refuse by name** rather than drop the overflow: a server whose
/// extra tool silently vanished would look like a server with a broken tool.
///
/// # This asserted two slots, briefly asserted one, and asserts two again — ADR-058
///
/// Splitting `write` out of `edit` took the exposed builtins to eleven, and against a cap of
/// twelve that left an MCP server exactly **one** tool. The arithmetic was correct and the
/// outcome was wrong: a fix to Marlowe's own surface had quietly been paid for out of a user's
/// server allowance, and one tool is not a usable budget for a server.
///
/// The cap moved to thirteen instead. **Two slots is the property being defended here**, not the
/// number twelve — so this test names the slots rather than the total, and a future builtin that
/// takes the count back to one fails it.
#[test]
fn two_mcp_tools_fit_the_budget_and_a_third_refuses_by_name() {
    let base = CapabilityProfile::interactive();
    let builtins = base.exposed_tools().len();
    assert_eq!(builtins, 12, "ADR-051 `use`; ADR-058 `write`; ADR-059 `glob`");

    // The property: whatever the builtins are, a server gets two.
    assert_eq!(
        MAX_EXPOSED_TOOLS - builtins,
        2,
        "an MCP server must keep two slots; a builtin that eats one is a decision, not a detail"
    );

    let two = CapabilityProfile::interactive_with(vec![
        ToolId::new("crm__lookup"),
        ToolId::new("crm__search"),
    ])
    .expect("two MCP tools fit the two remaining slots");
    assert_eq!(two.exposed_tools().len(), MAX_EXPOSED_TOOLS, "exactly at the budget");
    assert!(two.exposed_tools().contains(&ToolId::new("crm__lookup")));

    let three = CapabilityProfile::interactive_with(vec![
        ToolId::new("crm__lookup"),
        ToolId::new("crm__search"),
        ToolId::new("crm__create"),
    ]);
    match three {
        Err(marlowe_loop::ProfileError::Exposure(ExposureError::TooMany { got })) => {
            assert_eq!(got, MAX_EXPOSED_TOOLS + 1, "the refusal carries the count to act on")
        }
        other => panic!(
            "one tool past the budget must refuse by name so the user can choose what to give \
             up; got {other:?}"
        ),
    }
}

/// A widened profile is still a **top-level** profile, and a spawn from it still narrows.
///
/// `interactive_with` is the only widening path in the codebase and this is why that is safe:
/// it takes no parent, so there is nothing for it to grow past. `WidenedPastParent` is untouched.
#[test]
fn widening_a_top_level_profile_does_not_weaken_the_narrowing_rule_for_children() {
    let parent = CapabilityProfile::interactive_with(vec![ToolId::new("crm__lookup")])
        .expect("one extra tool fits");

    assert!(parent
            .narrowed(
                vec![ToolId::new("read")],
                marlowe_loop::AgentLevel::Worker,
                marlowe_loop::ModelRoute::Worker,
            )
            .is_ok(), "a child may narrow");
    assert!(
        parent
            .narrowed(
                vec![ToolId::new("crm__never_registered")],
                marlowe_loop::AgentLevel::Worker,
                marlowe_loop::ModelRoute::Worker,
            )
            .is_err(),
        "a child may not name a tool the parent does not have"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 3. A malformed mcp.json
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **ADR-052 §6.** Starting with the servers a broken parse happened to reach would give the user
/// half their tools and no statement that anything went wrong.
#[test]
fn a_malformed_mcp_config_is_an_error_and_an_absent_one_is_not() {
    let dir = std::env::temp_dir()
        .join(format!("marlowe-mcpcfg-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Absent: no servers, no error. A profile without MCP is not a fault.
    assert_eq!(
        marlowe_daemon::mcp::read_config(&dir).expect("absent is not an error").len(),
        0
    );

    // Malformed: refused, and the message names the file.
    std::fs::write(dir.join("mcp.json"), "{ this is not a list of server specs").unwrap();
    let err = marlowe_daemon::mcp::read_config(&dir)
        .expect_err("a malformed config must refuse rather than yield an empty list");
    assert!(err.contains("mcp.json"), "the refusal must name the file: {err}");

    // The control: a WELL-FORMED file at the same path parses, so the refusal above is about the
    // content and not about the path, the permissions, or the reader.
    std::fs::write(
        dir.join("mcp.json"),
        r#"[{"id":"crm","command":"echo","args":["hi"]}]"#,
    )
    .unwrap();
    let specs = marlowe_daemon::mcp::read_config(&dir).expect("a well-formed config parses");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, "crm");
    assert!(
        specs[0].consequence.is_none(),
        "an undeclared consequence stays None so `load` applies §7.3's absent-means-maximum"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn allowed(tool: &str) -> Adjudication {
    let tool = ToolId::new(tool);
    Adjudication {
        decision: marlowe_permission::PermissionDecision {
            id: marlowe_permission::DecisionId(1),
            tool: tool.clone(),
            action_class: marlowe_permission::ActionClass {
                tool,
                shape: 0,
                label: "test".into(),
            },
            outcome: marlowe_permission::Outcome::Allowed,
            blast_radius: marlowe_permission::BlastRadius {
                verb: "web".into(),
                scope: "test".into(),
                reversible: true,
                novelty: None,
            },
            taint: marlowe_permission::TaintSet::new(),
            reasons: Vec::new(),
        },
        handles: Default::default(),
    }
}
