//! **What the model is actually told about each parameter, read off the outbound body.**
//!
//! `builtin.rs`'s own guard asserts that every parameter carries a `description`. That is a check
//! on the DECLARATION, and this project has a standing count of properties asserted where they are
//! declared rather than where they are enforced — `web`'s `inline_threshold_bytes: 0` with no
//! reader, `persona/v1.md` loaded but never in a request body. A description that is stored and
//! never serialised would satisfy the declaration guard exactly.
//!
//! So this reads `request_body`'s `tools` array: the bytes `/api/chat` receives.
//!
//! The failure it exists for was live. Journal seq 4677–4679, 2026-08-26: the model called `read`
//! with neither `path` nor `ref`. Both are optional, and `param_description` generated the same
//! sentence for both — **"Optional. text."** — which reads as documentation and carries none.

use marlowe_loop::{Assembler, Block, CallLimits, SessionId, SessionState, SourceKind};
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, BUILTIN_TOOLS};

/// The generated fallback, verbatim from `param_description`. If this string is what a parameter
/// gets, nothing was written for it.
const GENERATED: [&str; 2] = ["REQUIRED. text.", "Optional. text."];

fn body() -> serde_json::Value {
    let driver = OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtin manifests load"),
    );
    // Every builtin at once, so no tool can be missed by an exposure this test chose.
    let tools = ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect())
        .expect("the builtins fit one exposed set");
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "do something".to_string(),
        marlowe_contract::TrustClass::UserAsserted,
    ));
    let view = Assembler::new(100_000, 10_000).assemble(&state);
    driver.request_body(&view, &tools, CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator })
}

#[test]
fn every_parameter_the_model_is_offered_carries_a_written_description() {
    let body = body();
    let tools = body["tools"].as_array().expect("the request offers tools");
    assert_eq!(
        tools.len(),
        BUILTIN_TOOLS.len(),
        "not every builtin reached the wire, so this test cannot have checked them all"
    );

    let mut bare = Vec::new();
    let mut checked = 0usize;
    for t in tools {
        let name = t["function"]["name"].as_str().unwrap_or("?");
        let props = t["function"]["parameters"]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`{name}` has no properties object on the wire"));
        for (param, spec) in props {
            checked += 1;
            let d = spec["description"].as_str().unwrap_or("");
            if d.trim().is_empty() || GENERATED.contains(&d.trim()) {
                bare.push(format!("{name}::{param} = {d:?}"));
            }
        }
    }

    // **The control on the loop itself.** Zero parameters checked would satisfy the assertion
    // below, and a `properties` shape that stopped serialising would produce exactly that.
    assert!(
        checked >= 15,
        "only {checked} parameters were found on the wire, so the check below is nearly vacuous"
    );
    assert!(
        bare.is_empty(),
        "these reach the model as a sentence generated from type and arity, which reads as \
         documentation and is not. Live, that is why `read` was called with no subject at all: \
         {bare:?}"
    );
}

/// The three facts a model most needs and could not previously have known, asserted individually
/// so a rewrite that keeps the descriptions non-empty but drops their content still fails.
///
/// Each is read off an executor: `write` replaces the whole file, `slice_lines` is 1-based
/// inclusive, and `grep` compiles its `pattern` with the `regex` crate.
///
/// **The first of these used to be about `edit::replacing` warning that omitting it overwrote
/// everything.** That warning is gone because the hazard is: `edit` has no whole-file mode any
/// more, and `write` is a separate tool. A description cannot be the last line of defence against
/// an ambiguity that has been removed from the shape.
#[test]
fn the_three_facts_that_cost_a_call_are_the_ones_actually_stated() {
    let body = body();
    let tools = body["tools"].as_array().expect("tools");
    let find_desc = |tool: &str, param: &str| -> String {
        tools
            .iter()
            .find(|t| t["function"]["name"].as_str() == Some(tool))
            .and_then(|t| t["function"]["parameters"]["properties"][param]["description"].as_str())
            .unwrap_or_else(|| panic!("`{tool}::{param}` is not on the wire"))
            .to_string()
    };

    // `write` is the destructive one now, and it says so where the content is named.
    let content = find_desc("write", "content");
    assert!(
        content.to_lowercase().contains("entire") && content.to_lowercase().contains("replaced"),
        "`write::content` does not say it replaces everything already in the file: {content:?}"
    );
    // And `edit` points at it, so a model that reaches for the wrong one is redirected rather
    // than refused into a guess. Live, the model reached for the SHELL.
    let edit_desc = find_desc("edit", "path");
    assert!(
        edit_desc.contains("`write`"),
        "`edit::path` does not name the tool that creates a file, so a model holding a create \
         request has nothing to follow: {edit_desc:?}"
    );
    let range = find_desc("read", "range");
    assert!(
        range.contains("1-based") && range.to_lowercase().contains("inclusive"),
        "`read::range`'s format is unstated, so the model has to guess whether it is 0- or \
         1-based and whether the end is included: {range:?}"
    );
    // **`exposed_tools` must deny the mechanism the model invented.**
    //
    // Observed live 2026-08-27: asked to summarise something, the model passed an EMPTY
    // `exposed_tools` and explained that the child "only needs read access, which the task will
    // handle internally through the tool grant mechanism". There is no such mechanism. The old
    // wording said *"you cannot GRANT what you do not have"* and *"reasons from `task` alone"*,
    // and between those two phrases the model built a second, imaginary route by which a child
    // could acquire tools -- then chose it over the real one.
    //
    // A description cannot enumerate every wrong belief. It CAN close the one that was actually
    // held, which is what this pins: the parameter says it is the only way, and says `task` is
    // not another one.
    let tools = find_desc("run", "exposed_tools");
    assert!(
        tools.contains("only way"),
        "`exposed_tools` must state that it is the ONLY way a child gets a tool: {tools:?}"
    );
    assert!(
        tools.contains("`task`"),
        "it must name `task` as the thing that CANNOT grant access, because that is the          alternative the model invented: {tools:?}"
    );

    // **INVERTED, ADR-059.** This asserted that `find::pattern` said *"NOT a regular
    // expression"*, which was true and is now the opposite of the shipped behaviour. Left as it
    // was, it would have stayed GREEN on a build where the description promised a literal
    // substring and the executor compiled a regex — a description asserted where it is written,
    // against an executor that had moved underneath it. What the model now has to know is what
    // the engine does NOT have, because a backreference fails to compile rather than being
    // ignored.
    let pattern = find_desc("grep", "pattern");
    let lower = pattern.to_lowercase();
    assert!(
        !lower.contains("not a regular expression"),
        "`grep::pattern` still claims it is not a regex, which is now false: {pattern:?}"
    );
    assert!(
        pattern.contains("(?i)"),
        "`grep::pattern` must name `(?i)`, because there is no case-insensitivity parameter and a \
         model that does not know the spelling cannot ask for it: {pattern:?}"
    );
    assert!(
        lower.contains("lookaround") && lower.contains("fail to compile"),
        "`grep::pattern` must say lookaround FAILS TO COMPILE — a model told only that it is a \
         regex will write `(?=` and read the refusal as the tool being broken: {pattern:?}"
    );
}
