//! M3-DESIGN §1's five agent levels, and the three rules that make them structural rather than
//! descriptive.
//!
//! # Every assertion here is about what a level PREVENTS, never about what it says
//!
//! `assert_eq!(profile.level(), AgentLevel::Master)` is instance #15 in one line: it reads
//! identically on a build where the level is a field nobody consults. So each rule below is
//! asserted as a **refusal**, and every refusal is paired with a **positive control** — the same
//! call at a level that may do it, succeeding. A test that only sees refusals cannot tell a
//! working rule from a constructor that refuses everything.
//!
//! The mutation that reddens each test is named on the test.

use marlowe_loop::{
    AgentLevel, Budget, CapabilityProfile, Disposition, InterruptPolicy, ModelRoute, ProfileError,
};
use marlowe_permission::EgressPolicy;
use marlowe_tools::{ExposedSet, ToolId};

fn set(names: &[&str]) -> ExposedSet {
    ExposedSet::new(names.iter().map(|n| ToolId::new(*n)).collect()).expect("a small set")
}

fn profile(
    names: &[&str],
    level: AgentLevel,
) -> Result<CapabilityProfile, ProfileError> {
    CapabilityProfile::new(
        set(names),
        EgressPolicy::DenyAll,
        InterruptPolicy::Unattended,
        ModelRoute::Worker,
        level,
        false,
        false,
    )
}

/// **§1.1's fatal defect, as a test.** The first design's headline assertion was that both
/// dispositions give the *same* answer, so it was green whether the disposition was plumbed
/// through or dropped on the floor in `SpawnRequest::from_args`.
///
/// *Mutation:* replace `parse_kind(text("kind"))` with `Disposition::Work` in
/// `driver.rs::from_args` — the third assertion below still passes (it is about `child_of`), and
/// `spawn_from_a_model_reply`'s receipt test is what reddens. *Mutation here:* make both
/// `Secretary` arms of `child_of` return the same value — the first assertion reddens.
#[test]
fn the_two_dispositions_do_not_produce_the_same_top_agent() {
    let manage = AgentLevel::child_of(AgentLevel::Secretary, Disposition::Manage).unwrap();
    let work = AgentLevel::child_of(AgentLevel::Secretary, Disposition::Work).unwrap();
    assert_ne!(
        manage, work,
        "§1.1's two words mean different grants: `master` gets a create grant, `worker` does the \
         task itself. If they produce the same value nothing downstream can tell them apart"
    );
    assert_eq!(manage, AgentLevel::TopAgent { manages: true });
    assert_eq!(work, AgentLevel::TopAgent { manages: false });

    // And the difference is enforced where it costs something: a top-agent spawned to WORK
    // creates nothing, and cannot hold the create grant.
    assert!(AgentLevel::child_of(AgentLevel::TopAgent { manages: false }, Disposition::Work).is_err());
    assert!(
        AgentLevel::child_of(AgentLevel::TopAgent { manages: false }, Disposition::Manage).is_err()
    );
    assert!(matches!(
        profile(&["run"], AgentLevel::TopAgent { manages: false }).unwrap_err(),
        ProfileError::CreateGrantNotHeldAtThisLevel { .. }
    ));
    // The positive control: the same set at the level that may hold it.
    assert!(profile(&["run"], AgentLevel::TopAgent { manages: true }).is_ok());
}

/// **§1.2 IS REVERSED: A MASTER MAY HOLD WORKING TOOLS. Only `ask` is withheld.**
///
/// This test asserted the opposite for one day. §1.2 said *"masters hold no working tools,
/// structurally"*, and the human overturned it on 2026-08-31: the PI is the senior researcher who
/// does the hardest part himself and spawns assistants because the job is larger than one context,
/// not because he is forbidden the work.
///
/// **The refusal that survives is a different decision that shared the same enforcement.**
/// `MANAGEMENT_TOOLS` withheld working tools *and* `ask` under one whitelist. Deleting it for the
/// first would have dropped the second in silence — so `ask` is now refused by name, because
/// §3.1 gives only a top-agent reach to the user and a question from level 3 has nobody to arrive
/// at.
///
/// *Mutation:* delete the `Master` guard in `CapabilityProfile::new` — the `ask` row reds.
/// *Mutation:* restore the working-tool refusal — every row in the first loop reds.
#[test]
fn a_master_may_hold_working_tools_and_may_not_hold_ask() {
    let working = [
        "bash", "read", "write", "edit", "glob", "grep", "web", "recall", "remember", "use",
    ];
    for t in working {
        assert!(
            profile(&[t], AgentLevel::Master).is_ok(),
            "`{t}` at level 3 must be constructible: the PI does the hardest part itself, and \n             §1.2's structural withholding was reversed on 2026-08-31"
        );
        // **The positive control, kept from the old test and inverted with it.** Without it this
        // loop is green on a constructor that admits everything at every level, which is a broken
        // product and a passing suite.
        assert!(
            profile(&[t], AgentLevel::Worker).is_ok(),
            "`{t}` at level 4 is the ordinary case and must be constructible"
        );
    }

    // `run` alongside working tools is the whole point: delegate AND do the work.
    assert!(profile(&["run", "edit", "bash"], AgentLevel::Master).is_ok());

    // **`ask` is not a master's** — the one refusal that survives the reversal.
    let refused = format!("{:?}", profile(&["ask"], AgentLevel::Master));
    assert!(
        refused.contains("OnlyATopAgentMayAsk"),
        "a master holding `ask` must be refused BY NAME; got {refused}"
    );
    // And it is refused even in company, so the check is not order-dependent.
    assert!(profile(&["run", "ask"], AgentLevel::Master).is_err());

    // **The control that keeps the refusal from being universal.** A top-agent DOES reach the
    // user, so `ask` there is the ordinary case; without this row a constructor that refused
    // `ask` at every level would pass.
    assert!(profile(&["ask"], AgentLevel::TopAgent { manages: true }).is_ok());
    assert!(profile(&["ask"], AgentLevel::TopAgent { manages: false }).is_ok());

    // ── the instance #17 control ────────────────────────────────────────────────────────────
    //
    // "A master may not edit" is expressed by the tool not being in the set. It must NOT be
    // expressed as a budget dimension of zero: `Budget::exhausted` compares `spent >= budget`,
    // so `0 >= 0` fires on the first iteration and the master pauses before its first model call
    // while looking perfectly configured. That is what silently stopped the quarantined reader
    // reading anything at all (ADR-041).
    let b = Budget::interactive();
    assert!(b.tool_calls >= 1, "a zero dimension is 'already exhausted', not 'may not use'");
    assert!(b.subagents >= 1);
    assert!(b.depth >= 1);
}

// **`management_tools_and_working_tools_are_disjoint_and_the_list_has_not_shrunk` WAS HERE.**
//
// It pinned `MANAGEMENT_TOOLS` literally, from outside the constant, so the list could not shrink
// unnoticed -- and it worked exactly once, catching `ask`'s removal on 2026-08-31 with the very
// mutation its own doc comment had predicted.
//
// **The constant it guarded no longer exists.** The human reversed §1.2 the same day: a master may
// hold working tools, so a whitelist of what a master MAY hold has nothing left to say, and `ask`
// is now refused by name at the `Master` arm rather than by absence from a list. A test pinning a
// deleted constant is not a weaker guard, it is a guard with no subject -- instance #14's shape --
// so it is removed with its reason rather than left asserting an empty set against itself.
//
// **What replaces it:** `a_master_may_hold_working_tools_and_may_not_hold_ask` above, whose `ask`
// row asserts the refusal BY NAME and whose top-agent rows are the control that keeps the refusal
// from being universal.

/// **The level table is total, and no model-initiated spawn can reach level 5.**
///
/// *Mutation:* add `(Master, Manage) => Ok(Master)` — the tree becomes unbounded and the
/// `Master` row reddens. *Mutation:* have any arm return `ToolSpawned` — the last loop reddens.
#[test]
fn the_level_table_is_total_and_never_produces_a_tool_spawned() {
    use AgentLevel::*;
    use Disposition::*;

    let levels =
        [Secretary, TopAgent { manages: true }, TopAgent { manages: false }, Master, Worker,
         ToolSpawned];

    let expected: &[((AgentLevel, Disposition), Option<AgentLevel>)] = &[
        ((Secretary, Manage), Some(TopAgent { manages: true })),
        ((Secretary, Work), Some(TopAgent { manages: false })),
        ((TopAgent { manages: true }, Manage), Some(Master)),
        ((TopAgent { manages: true }, Work), Some(Worker)),
        ((TopAgent { manages: false }, Manage), None),
        ((TopAgent { manages: false }, Work), None),
        ((Master, Manage), None),
        ((Master, Work), Some(Worker)),
        ((Worker, Manage), None),
        ((Worker, Work), None),
        ((ToolSpawned, Manage), None),
        ((ToolSpawned, Work), None),
    ];
    assert_eq!(expected.len(), levels.len() * 2, "every (level, disposition) pair is covered");

    for ((parent, d), want) in expected {
        assert_eq!(
            AgentLevel::child_of(*parent, *d).ok(),
            *want,
            "child_of({parent:?}, {d:?})"
        );
    }

    for parent in levels {
        for d in [Manage, Work] {
            assert_ne!(
                AgentLevel::child_of(parent, d).ok(),
                Some(ToolSpawned),
                "level 5 is spawned by a TOOL. No model call may produce one — it is withheld \
                 structurally, by this function having no arm for it, not by a counter"
            );
        }
    }
}

/// **Every named constructor declares its level, and a sixth variant fails to COMPILE here.**
///
/// The `match` has no wildcard, so the coverage of this check is not a list it maintains
/// (#19-safe). *Mutation:* change `interactive()` to `AgentLevel::Worker` — the assertion reds,
/// and so does `CapabilityProfile::new`, because `interactive()` holds `run`.
#[test]
fn every_named_profile_declares_its_level() {
    let named: [(&str, CapabilityProfile); 3] = [
        ("quarantined_reader", CapabilityProfile::quarantined_reader()),
        ("consolidation", CapabilityProfile::consolidation()),
        ("interactive", CapabilityProfile::interactive()),
    ];
    for (name, p) in &named {
        // No wildcard. A sixth `AgentLevel` variant is a compile error here.
        let expected_name = match p.level() {
            AgentLevel::Secretary => "interactive",
            AgentLevel::TopAgent { .. } => "(no named constructor builds a top-agent)",
            AgentLevel::Master => "(no named constructor builds a master)",
            AgentLevel::Worker => "consolidation",
            AgentLevel::ToolSpawned => "quarantined_reader",
        };
        assert_eq!(&expected_name, name, "`{name}` declares the wrong level");
    }

    // `interactive_with` inherits: an MCP server contributes tools, not a position in the tree.
    let with = CapabilityProfile::interactive_with(vec![ToolId::new("crm__lookup")]).unwrap();
    assert_eq!(with.level(), AgentLevel::Secretary);
}

/// **The create grant is holding `run`, and there is exactly one definition of that.**
///
/// *Mutation:* reintroduce a `level.may_hold_create_grant()` conjunct inside
/// `may_create_agents` — this reds at `TopAgent { manages: false }` and `Worker`, where the
/// constructor already guarantees `run` is absent and the conjunct can only disagree with it.
#[test]
fn a_profile_that_may_create_agents_is_exactly_one_that_holds_run() {
    let cases = [
        (AgentLevel::Secretary, true),
        (AgentLevel::TopAgent { manages: true }, true),
        (AgentLevel::TopAgent { manages: false }, false),
        (AgentLevel::Master, true),
        (AgentLevel::Worker, false),
        (AgentLevel::ToolSpawned, false),
    ];
    for (level, may_hold) in cases {
        let with_run = profile(&["run"], level);
        assert_eq!(
            with_run.is_ok(),
            may_hold,
            "constructing a {level:?} holding `run` should be {may_hold}"
        );
        if let Ok(p) = with_run {
            assert!(p.may_create_agents(), "it holds `run`");
            assert_eq!(p.may_create_agents(), p.exposed_tools().contains(&ToolId::new("run")));
        }
    }

    // Without `run`, every level answers false — including the ones that MAY hold it.
    for (level, _) in cases {
        if matches!(level, AgentLevel::ToolSpawned) {
            continue; // level 5 holds nothing at all; covered by the constructor test below
        }
        let p = profile(&["ask"], level);
        if let Ok(p) = p {
            assert!(
                !p.may_create_agents(),
                "{level:?} without `run` does not hold the create grant"
            );
        }
    }
}

/// **A tool-spawned agent holds nothing, and this is strictly wider than the quarantine check.**
///
/// SCOPED-MEMORY §4's fact extractor is a level-5 agent that does *not* set `reads_untrusted`,
/// so `QuarantineWithTools` would not stop it being constructed with tools. *Mutation:* delete
/// the `ToolSpawned` arm — the first assertion reds while every quarantine test stays green.
#[test]
fn a_tool_spawned_agent_holds_no_tools_even_when_it_reads_nothing_untrusted() {
    match profile(&["read"], AgentLevel::ToolSpawned).unwrap_err() {
        ProfileError::ToolSpawnedWithTools { count } => assert_eq!(count, 1),
        other => panic!("expected ToolSpawnedWithTools, got {other:?}"),
    }
    // The positive control, and the shape layer 1 actually ships.
    assert!(profile(&[], AgentLevel::ToolSpawned).is_ok());
    assert_eq!(CapabilityProfile::quarantined_reader().level(), AgentLevel::ToolSpawned);
}

/// **#12: `serde` is a way in, and a level must not be widenable through it.**
///
/// *Mutation:* add `#[serde(default)]` to `Raw.level` — the absent case greens, and a checkpoint
/// with no level silently deserializes as whatever the default is. *Mutation:* replace the
/// hand-written `Deserialize` with a derive — both cases green.
#[test]
fn a_level_cannot_be_widened_by_deserialization() {
    // **The payload changed with the rule it tests, 2026-08-31.** It was a master holding `edit`,
    // which the section 1.2 reversal made legal — so the old payload deserialized cleanly and this
    // test failed pointing at a profile the product is now supposed to build. The property under
    // test is unchanged (`serde` routes through the validating constructor); only the refusal it
    // rides on moved, from the working-tool whitelist to `OnlyATopAgentMayAsk`.
    let master_with_ask = r#"{
        "exposed_tools": ["ask"],
        "egress": "deny_all",
        "interrupt": "unattended",
        "model_route": "worker",
        "level": "master",
        "may_write_memory": false,
        "reads_untrusted": false
    }"#;
    let err = serde_json::from_str::<CapabilityProfile>(master_with_ask).unwrap_err().to_string();
    assert!(
        err.contains("only a top-agent reaches the user"),
        "the refusal must name the rule it is enforcing: {err}"
    );

    // **The control that keeps this from passing on a constructor that refuses every master.** A
    // master holding working tools is now the ordinary case and must deserialize cleanly; without
    // this row, restoring the old whitelist would leave the assertion above green.
    let master_with_edit = r#"{
        "exposed_tools": ["edit", "run"],
        "egress": "deny_all",
        "interrupt": "unattended",
        "model_route": "worker",
        "level": "master",
        "may_write_memory": false,
        "reads_untrusted": false
    }"#;
    assert!(
        serde_json::from_str::<CapabilityProfile>(master_with_edit).is_ok(),
        "a master doing the work is the PI model and must round-trip"
    );

    let no_level = r#"{
        "exposed_tools": ["read"],
        "egress": "deny_all",
        "interrupt": "unattended",
        "model_route": "worker",
        "may_write_memory": false,
        "reads_untrusted": false
    }"#;
    let err = serde_json::from_str::<CapabilityProfile>(no_level).unwrap_err().to_string();
    assert!(err.contains("level"), "an absent level must fail by name, not default: {err}");

    // The positive control: a well-formed profile still round-trips.
    let ok = r#"{
        "exposed_tools": ["read"],
        "egress": "deny_all",
        "interrupt": "unattended",
        "model_route": "worker",
        "level": "worker",
        "may_write_memory": false,
        "reads_untrusted": false
    }"#;
    let p = serde_json::from_str::<CapabilityProfile>(ok).expect("a worker holding `read`");
    assert_eq!(p.level(), AgentLevel::Worker);

    // And a round trip through the SERIALIZED form of a real profile, so the two sides cannot
    // disagree about the spelling.
    let json = serde_json::to_string(&CapabilityProfile::interactive()).unwrap();
    assert!(json.contains("\"level\":\"secretary\""), "{json}");
    let back: CapabilityProfile = serde_json::from_str(&json).unwrap();
    assert_eq!(back.level(), AgentLevel::Secretary);
}
