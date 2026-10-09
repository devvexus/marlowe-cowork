//! The tool-call reliability probe. **This is how the default model is chosen.**
//!
//! ADR-028 requirement 2: a local model is materially weaker than a frontier one, and the
//! difference bites hardest on tool calling — which is exactly where a debugging session cannot
//! tell a harness bug from a model that cannot follow a schema, because the symptom is identical:
//! a tool that did not run.
//!
//! So the default is picked on a **measured rate with a denominator**, not on reputation. The
//! first thing a user does is ask Marlowe to read a file; a model that cannot emit a well-formed
//! tool call will look like a broken harness.
//!
//! # Running it
//!
//! ```text
//! ollama serve                       # in another terminal
//! cargo test -p marlowe-provider --test tool_call_probe -- --ignored --nocapture
//! ```
//!
//! `#[ignore]` because it needs a live endpoint and takes minutes. It is **not** part of the
//! ordinary suite, and that is deliberate: a test that silently passed when the endpoint was
//! down would report a measurement that never happened.
//!
//! # What is measured, and the distinction that matters
//!
//! Two rates, because two failures look the same from outside and have different causes:
//!
//! - **well-formed** — the model emitted a tool call naming a registered tool with arguments
//!   that parse. Failing this is a model that cannot follow the schema.
//! - **correct target** — the *target argument* was also right. A model that calls `read` with
//!   the wrong path is well-formed and useless.
//!
//! A single "success rate" would blur them, and the blur is the thing a debugging session needs
//! unblurred.

use marlowe_loop::{CallLimits, ModelDriver, ModelStep};
use marlowe_provider::{LocalEndpoint, OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

/// Prompts that should each produce one obvious tool call. Deliberately plain: this measures
/// whether a model can emit a call at all, not whether it can be coaxed into one.
const TRIALS: &[(&str, &str, &str, &str)] = &[
    // (prompt, expected tool, target param, expected target)
    ("Read the file src/main.rs.", "read", "path", "src/main.rs"),
    ("Show me what is in README.md.", "read", "path", "README.md"),
    ("Open Cargo.toml and tell me what is in it.", "read", "path", "Cargo.toml"),
    // **These three used to name `find`, and one of them was measuring a coin flip.** *"Find
    // every occurrence of TODO"* is the English word that means FILENAMES, used to elicit a
    // CONTENTS search — see ADR-059. The tool is `grep` now and the prompts say what they want.
    ("Search the project for the word `budget`.", "grep", "pattern", "budget"),
    ("Find every occurrence of TODO inside the files.", "grep", "pattern", "TODO"),
    ("Look for the string `invariant` in the code.", "grep", "pattern", "invariant"),
    // **The measurement that would overturn `(?i)`-over-a-Boolean.** ADR-059 rejected an
    // `ignore_case` parameter on the grounds that `ollama.rs` coerces the model's JSON — a real
    // `Bool` becomes `Boolean`, the string `"true"` becomes `Text("true")` — so a Boolean from a
    // 9B model is a coin flip on the wire, and `(?i)` costs no schema. That is an argument, and
    // these two trials are the number that settles it: they pass when the model reaches for
    // `(?i)` and fail when it invents a parameter, which is exactly what the executor refuses.
    ("Find `todo` in the code, ignoring case.", "grep", "pattern", "(?i)todo"),
    ("Case-insensitively search for the word `error`.", "grep", "pattern", "(?i)error"),
    ("Write the text `hello` into notes.txt.", "edit", "path", "notes.txt"),
    ("Create a file called out.txt containing `done`.", "edit", "path", "out.txt"),
    ("Run the command `cargo test`.", "bash", "command", "cargo test"),
    ("Execute `ls -la` in the shell.", "bash", "command", "ls -la"),
    ("What do you remember about my database?", "recall", "query", ""),
    ("Look up what I said about deadlines.", "recall", "query", ""),
];

/// **One model, unless a sweep is explicitly asked for.**
///
/// Model comparison on this project is bounded by local hardware: the development machine cannot
/// hold several large models resident at once, and a probe that iterated a list would try. The
/// default is therefore a single model, and a sweep needs `MARLOWE_PROBE_SWEEP=1` **and** an
/// explicit list — two deliberate acts, because the failure mode is a machine thrashing rather
/// than an error message.
///
/// This constraint is not a property of the probe. It binds every future routing decision,
/// including ADR-008's strong-model/cheap-model split, and `STATE.md` carries it so it does not
/// surface as a surprise when someone proposes one.
fn candidates() -> Vec<String> {
    let listed: Vec<String> = std::env::var("MARLOWE_PROBE_MODELS")
        .map(|s| s.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect())
        .unwrap_or_default();

    match listed.len() {
        0 => vec![marlowe_provider::DEFAULT_MODEL.to_string()],
        1 => listed,
        n => {
            assert!(
                std::env::var("MARLOWE_PROBE_SWEEP").is_ok(),
                "{n} models were listed. A sweep loads them one after another and this project's                  development machine cannot hold several large models at once — set                  MARLOWE_PROBE_SWEEP=1 if the machine running this can. Measuring one model at a                  time is the supported path."
            );
            listed
        }
    }
}

#[test]
#[ignore = "needs a live Ollama endpoint; run with --ignored"]
fn measure_tool_call_reliability() {
    let endpoint = LocalEndpoint::default_ollama();
    let registry = builtin_registry().unwrap();
    let exposed = ExposedSet::new(
        // **`done` was here and is no longer a builtin** — `ExposedSet::new` checks arity and
        // duplicates only, so it does not catch a name that is not registered, and this test is
        // `#[ignore]`d, which is why nobody saw it. Taken from `BUILTIN_TOOLS` names that exist.
        ["read", "edit", "grep", "bash", "recall"].iter().map(|t| ToolId::new(*t)).collect(),
    )
    .unwrap();

    println!("\n  tool-call reliability · {} trials per model", TRIALS.len());
    println!("  {:<22} {:>12} {:>14} {:>10}", "model", "well-formed", "correct target", "median");
    println!("  {}", "─".repeat(62));

    let mut rows = Vec::new();
    for model in candidates() {
        let Ok(routing) = Routing::uniform(&model) else {
            println!("  {model:<22} REFUSED (cloud tag)");
            continue;
        };
        let mut driver = OllamaDriver::new(endpoint.clone(), routing, builtin_registry().unwrap());
        let availability = driver.availability();
        if !availability.is_ready() {
            println!("  {model:<22} SKIPPED — {}", availability.remedy());
            continue;
        }

        let mut well_formed = 0u32;
        let mut correct_target = 0u32;
        let mut times: Vec<u64> = Vec::new();

        for (prompt, want_tool, target_param, want_target) in TRIALS {
            let view = probe_view(prompt);
            let call = driver.call(&view, &exposed, CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator });

            // Duration comes from OLLAMA'S OWN `total_duration`, not from a clock read here.
            // `marlowe/tests/determinism_guard.rs` fences real-clock reads across the whole
            // workspace, and it caught an `Instant::now()` in this file. Weakening the fence to
            // accommodate a probe would be modifying the guard to suit the thing it guards; the
            // response already carries the number, so no clock is needed.
            if let Ok(c) = &call {
                times.push(c.usage.wall_ms);
            }

            match call.map(|c| c.step) {
                // The probe measures whether the model names the right tool with the right
                // target. A batch is scored on its FIRST call — the probe's prompts each ask for
                // one action, so a batch here would itself be a finding, and scoring the first is
                // what keeps the measurement comparable with the 12/12 baseline in `DEFAULT_MODEL`.
                Ok(ModelStep::ToolCall { calls }) if !calls.is_empty() => {
                    let tool = &calls[0].tool;
                    let args = &calls[0].args;
                    let named_right = tool.as_str() == *want_tool;
                    if named_right {
                        well_formed += 1;
                        let got = args
                            .get(target_param)
                            .and_then(marlowe_permission::ArgValue::as_text)
                            .unwrap_or_default();
                        if want_target.is_empty() || got.contains(want_target) {
                            correct_target += 1;
                        }
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }

        times.sort_unstable();
        let median_ms = times.get(times.len() / 2).copied().unwrap_or(0);
        let n = TRIALS.len() as u32;
        println!(
            "  {model:<22} {:>7}/{:<4} {:>9}/{:<4} {:>8}ms",
            well_formed, n, correct_target, n, median_ms
        );
        rows.push((model, well_formed, correct_target, n, median_ms));
    }

    println!();
    if let Some((best, wf, _, n, _)) =
        rows.iter().max_by_key(|(_, wf, ct, _, _)| (*wf, *ct)).cloned()
    {
        println!("  BEST: {best} at {wf}/{n} well-formed");
        println!("  Pin it as `marlowe_provider::DEFAULT_MODEL` only if it clears the bar in");
        println!("  `capability::MIN_RATE` ({:.0}%) over at least {} trials.",
                 marlowe_provider::capability::MIN_RATE * 100.0,
                 marlowe_provider::capability::MIN_TRIALS);
    } else {
        panic!(
            "no model was measured. Is `ollama serve` running, and are the models in \
             MARLOWE_PROBE_MODELS pulled? A probe that measured nothing must fail rather than \
             report an empty table."
        );
    }
}

/// A minimal context view: identity, one governance line, and the user's prompt.
fn probe_view(prompt: &str) -> marlowe_loop::ContextView {
    use marlowe_contract::TrustClass;
    use marlowe_loop::{Assembler, Block, GovernanceConstraint, SessionId, SessionState, SourceKind};

    let mut state = SessionState::new(
        SessionId::from_name("probe"),
        "You are Marlowe. Use a tool when the user asks for something a tool can do.",
    );
    state.assert_governance(GovernanceConstraint::asserted("The workspace is the current directory."));
    state.push(Block::new(SourceKind::History, prompt, TrustClass::UserAsserted));
    Assembler::new(32_000, 2_000).assemble(&state)
}

#[test]
fn the_probe_refuses_to_report_a_rate_it_did_not_measure() {
    // The guard on the guard. `measure_tool_call_reliability` panics when nothing was measured,
    // rather than printing an empty table that reads like a clean run — the same shape as a
    // traversal class that silently did not run.
    assert!(TRIALS.len() >= marlowe_provider::capability::MIN_TRIALS as usize,
        "the trial set must be at least MIN_TRIALS, or a passing probe cannot clear the bar it \
         is measured against");
}
