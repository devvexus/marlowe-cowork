//! **The two shapes an OpenAI-dialect endpoint refuses, in one place because two adapters build
//! the same request.**
//!
//! Neither of these is a style preference. Each is a hard `400` from a strict endpoint, and each
//! was produced by code that is *correct everywhere else in the system* — which is why they were
//! invisible until a hosted provider was pointed at the quarantined reader.
//!
//! # 1. `tools: []`
//!
//! The dialect says `tools` may be absent, or an array with **at least one** element. An empty
//! array is a schema violation, not "no tools".
//!
//! Two live producers, and neither looks like a bug at its own site:
//!
//! * `CapabilityProfile::quarantined_reader` is `ExposedSet::empty()`. That is **layer 1** — the
//!   load-time invariant that a component which reads untrusted content cannot hold a tool. The
//!   security property is exactly right, and rendering it onto the wire ended every quarantined
//!   read on the hosted path.
//! * `FARMING_HARD_STOP` withholds tools for one call, on purpose, from the **parent**: *"a nudge
//!   asks; an empty tool set removes the option."* Same wire shape, no quarantine involved.
//!
//! Omitting the field removes the option just as an empty array was meant to, and is what the
//! dialect actually defines.
//!
//! # 2. A `tool` message that answers no call
//!
//! `role: "tool"` is a **reply**. The dialect pairs it to an assistant `tool_calls` entry by id,
//! and a `tool` message whose `tool_call_id` is missing — or names a call that is not in the
//! request — is refused.
//!
//! The producer here is `Engine::condense_batch`, which pushes each fetched page into the child's
//! window as `SourceKind::ToolResults`. In the *parent's* conversation those blocks are genuine
//! replies to genuine calls. In the **child's** they are not: the child never called anything and
//! structurally never can, so the block carries no `wire` metadata and there is no assistant turn
//! in front of it. The child's whole request was `system, assistant, tool` — a reply to a call
//! that does not exist, and no user message at all.
//!
//! A block that is a tool result *somewhere* is therefore not automatically a `tool` message
//! *here*, and the same orphaning is reachable any time trimming drops an assistant turn while
//! keeping its results. [`unorphan_tool_messages`] decides it from the request being built rather
//! than from the block's provenance, so it holds for a producer nobody has written yet.

use serde_json::Value;
use std::collections::BTreeSet;

/// The role an unlinkable tool result is demoted to.
///
/// `user` because it is the only role in the dialect that carries content and is not a claim
/// about who acted. **The content is not rewritten** — no prefix, no wrapper, no interpolation:
/// the harness already writes the label immediately before the bytes it names, and adding a
/// second frame here would be a second place for that framing to drift.
const DEMOTED_ROLE: &str = "user";

/// `tools`, or nothing at all.
///
/// Returns `None` for an empty tool set, so the caller omits the key rather than sending `[]`.
/// Takes the built array so there is one definition of "empty" and it is the one the wire sees —
/// asking the `ExposedSet` instead would be a different question, and the two can disagree: a
/// tool that is exposed but absent from the registry is filtered out while building the array,
/// leaving a non-empty set and an empty array.
pub fn tools_field(schema: Value) -> Option<Value> {
    match schema.as_array() {
        Some(a) if a.is_empty() => None,
        _ => Some(schema),
    }
}

/// Rewrite any `tool` message that answers no call in this request into a plain user message.
///
/// A `tool` message keeps its role only when it carries a `tool_call_id` that some **earlier**
/// assistant message in this same list announced. Order matters: a reply cannot precede its call.
/// Everything else is demoted to [`DEMOTED_ROLE`] with the pairing fields removed, because a
/// `tool_call_id` on a non-`tool` message is itself malformed.
pub fn unorphan_tool_messages(messages: &mut [Value]) {
    let mut announced: BTreeSet<String> = BTreeSet::new();
    for msg in messages.iter_mut() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("").to_string();

        if role == "tool" {
            let linked = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| announced.contains(id));
            if !linked {
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("role".into(), Value::String(DEMOTED_ROLE.into()));
                    obj.remove("tool_call_id");
                    obj.remove("tool_name");
                }
            }
            continue;
        }

        // Collected AFTER the `tool` branch and only from assistant turns, so a demoted message
        // cannot announce anything and a result can never license itself.
        if role == "assistant" {
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        announced.insert(id.to_string());
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The OpenAI dialect, built ONCE — ADR-060 §6
// ─────────────────────────────────────────────────────────────────────────────────────────
//
// # Why these moved here rather than being copied a third time
//
// Two drivers already speak this dialect (`marlowe-openrouter`, and now `llamacpp`) and a third
// builds the Ollama variant beside it. The message builder existed twice
// (`ollama.rs` / `openrouter/driver.rs`) and the tool-schema builder existed twice, and the two
// copies of `param_description` **had already drifted**: the Ollama one honours a parameter's own
// `description` and the OpenRouter one never did, so every hosted request described `run`'s `task`
// as "text" while the local one described what it is for. Nobody edited a copy wrongly; one copy
// was improved and the other was not, which is the whole failure mode.
//
// A third copy is how three drivers end up with three ideas of what a `tool` message is.

/// Build the `messages` array for an **OpenAI-dialect** endpoint, orphaned tool results already
/// demoted.
///
/// # The four differences from Ollama's `/api/chat`, each one load-bearing
///
/// 1. `arguments` is a **JSON string**, not an object. A server given an object here refuses the
///    request, and the refusal arrives as a 400 on the turn *after* a tool ran, which reads like a
///    tool bug.
/// 2. Every assistant tool call carries `"type": "function"`. Omitting it is an HTTP 500 from
///    `llama-server` — `Failed to parse messages: Missing tool call type` — on **iteration 2 of
///    every tool-using turn**, measured against the shipped body. The first call succeeds and the
///    second dies, which is the seam class nothing that tests halves can see.
/// 3. No `tool_name`; the pairing is `tool_call_id` alone.
/// 4. No `thinking`. Reasoning is not re-sent in this dialect.
///
/// **The shape below is the one BOTH servers accept**, and that is a measured claim rather than an
/// inference from the specification. ADR-060's probe ran all four candidate shapes against both
/// servers: today's Ollama shape (id, no type, object arguments) is an HTTP 500 on `llama-server`,
/// and the full OpenAI shape with string arguments is an HTTP 400 on Ollama
/// (`Value looks like object, but can't find closing '}' symbol`). So *"just send OpenAI"* is the
/// wrong instinct for the Ollama adapter and the right one here, and neither adapter may adopt the
/// other's.
pub fn openai_messages(view: &marlowe_loop::ContextView) -> Vec<Value> {
    use marlowe_loop::SourceKind;

    let mut messages: Vec<Value> = Vec::new();

    // ── ONE system message, at position 0. A chat-template constraint, not a preference. ──
    //
    // Several newer templates accept exactly one system message and require it first; qwen3.5's
    // tolerates more, which is why emitting one per block went unnoticed until a qwen3-next model
    // answered `Jinja Exception: System message must be at the beginning`. Injected memory joins
    // it rather than sitting mid-conversation: on the wire it is context, not a turn.
    // **INJECTED MEMORY IS NO LONGER IN HERE, AND THAT IS THE TTFT FIX.**
    //
    // It used to be chained onto the end of this message — which is **position 0**. A server reuses
    // its KV cache only for a **byte-identical prefix**, and memory is re-retrieved every turn, so
    // a single changed fact invalidated the system prompt *and every turn of conversation behind
    // it*. Measured on this machine: a **148-byte** change at the front of the prompt cost
    // **1.45 seconds**, spontaneously, in an ordinary run nobody was provoking. Over ten turns the
    // prompt-evaluation cost climbed **689 → 1,320 ms**, monotone in every one of six passes.
    //
    // The counterfactual — the same content moved *after* the history — measured **flat at ~275 ms
    // regardless of turn count**. That is why this moves rather than being trimmed or cached: the
    // only lever a client has on a server-side KV cache is **emitting byte-identical bytes**, and
    // it is achieved by construction or not at all.
    //
    // What stays true: `stable` then `context` in that order, one system message at position 0.
    let system: Vec<&str> = view
        .stable
        .iter()
        .chain(view.context.iter())
        .map(|b| b.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .collect();
    if !system.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system.join("\n\n"),
        }));
    }

    // **The memory that used to sit at position 0 is emitted at the TAIL instead — see below the
    // loop.** Collected first so the loop stays a straight projection of the tier.
    let recalled: Vec<&str> = view
        .volatile
        .iter()
        .filter(|b| b.source == SourceKind::InjectedMemory)
        .map(|b| b.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .collect();

    for block in view.volatile.iter() {
        let role = match block.source {
            // See `Engine::spawn`: a child's return is announced by an assistant turn and paired
            // by id, so it goes out as a tool result, never as the parent's own words.
            SourceKind::ToolResults | SourceKind::ChildResults => "tool",
            // See `SourceKind::Brief`: the parent speaking, not the child.
            SourceKind::Brief => "user",
            SourceKind::History => match block.trust {
                marlowe_contract::TrustClass::AgentInferred => "assistant",
                _ => "user",
            },
            SourceKind::InjectedMemory => continue,
            _ => "user",
        };
        let mut msg = serde_json::json!({ "role": role, "content": block.text });
        if let Some(w) = &block.wire {
            if !w.tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(
                    w.tool_calls
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "id": c.id,
                                // Difference 2. See this function's header.
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    // Difference 1. See this function's header.
                                    "arguments": c.arguments.to_string(),
                                }
                            })
                        })
                        .collect(),
                );
            }
            if let Some(id) = &w.tool_call_id {
                msg["tool_call_id"] = Value::String(id.clone());
            }
        }
        messages.push(msg);
    }

    // ── INJECTED MEMORY, AT THE TAIL ────────────────────────────────────────────────────────
    //
    // **Here rather than at position 0, and on the rails a spawned child's result already uses.**
    // See the note on the system message for the measurement; this is the other half of it. What
    // matters is that everything BEFORE this point is byte-identical between two turns of the same
    // session, so the server's KV cache survives and only the tail is re-evaluated.
    //
    // **Why not simply a `user` message.** A recalled fact would then be indistinguishable from
    // something the person just said — the model cannot tell "you told me this in March" from "you
    // are telling me this now", and acting on the second when it was the first is the whole reason
    // memory carries provenance. **Why not a mid-conversation `system` message.** A qwen3-next
    // template refuses it outright: `System message must be at the beginning`.
    //
    // So it arrives the way a child's return does: an assistant turn announcing a `recall`, paired
    // by id with a `tool` message carrying the text. `recall` is a real tool in the exposed set, so
    // this is not a fiction — it is the shape the model already understands for "the harness went
    // and got something".
    //
    // The trust class is untouched. `SourceKind::InjectedMemory` still identifies these blocks,
    // `trust_floor` is a `min` over blocks and reads neither the source kind nor the wire role, and
    // `blocks_composed_targets` therefore sees exactly what it saw before. **Layers 2 and 3 are
    // unchanged by construction, not by argument.**
    if !recalled.is_empty() {
        let id = "recall_injected_memory";
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": { "name": "recall", "arguments": "{}" },
            }],
        }));
        messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": id,
            "content": recalled.join("\n\n"),
        }));
    }

    // A `tool` message that answers no call is refused by a strict endpoint, and the quarantined
    // reader's window is nothing but such messages. See this module's header.
    unorphan_tool_messages(&mut messages);
    messages
}

/// The exposed tools, in the OpenAI function schema. **One definition for every driver.**
///
/// Only exposed tools are described — registration is unlimited, exposure is what the model sees
/// (§7.2).
///
/// `required` is not optional: omitting it makes every parameter optional, so a model that leaves
/// out the one thing the tool needs produces a call that is valid against the schema it was given
/// and is then refused by the permission layer for "no declared target" — a failure caused by our
/// own schema and reported as if the model had erred. See ADR-034.
///
/// **`required`, not `role`.** These are two questions and they were once one switch:
/// `ArgumentRole::Target` says what untrusted content may never shape, which is not the same as
/// what the tool cannot run without.
pub fn openai_tool_schema(
    registry: &marlowe_tools::ToolRegistry,
    exposed: &marlowe_tools::ExposedSet,
) -> Value {
    let tools: Vec<Value> = exposed
        .iter()
        .filter_map(|id| registry.get(id))
        .map(|reg| {
            let properties: serde_json::Map<String, Value> = reg
                .manifest
                .params()
                .iter()
                .map(|p| {
                    (
                        p.name.clone(),
                        serde_json::json!({
                            "type": json_type(p.ty),
                            "description": param_description(p),
                        }),
                    )
                })
                .collect();
            let required: Vec<&str> = reg
                .manifest
                .params()
                .iter()
                .filter(|p| p.required)
                .map(|p| p.name.as_str())
                .collect();
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": reg.id.as_str(),
                    // **ADR-052: this text is TRUSTED, and what the tool returns is not.**
                    // The user installed the server, which is the authorization decision, and the
                    // agent cannot install one — so this prose may direct action. Its RESULTS may
                    // not: they arrive `UntrustedContent` and go through layer 1 like any other
                    // untrusted result. `Description::new` has already bounded it and run it
                    // through `marlowe_contract::text`, not as a filter (§8.1 says filtering does
                    // not work) but so the description cannot render as something other than what
                    // the user read when they installed it.
                    "description": reg.description.text(),
                    "parameters": {
                        "type": "object",
                        "properties": properties,
                        "required": required,
                    },
                }
            })
        })
        .collect();
    Value::Array(tools)
}

/// The JSON Schema type for a parameter. **Not always `string`.**
///
/// Sending `"type": "string"` for an integer invites the model to quote it, and a quoted number
/// then falls to the trust floor as model-composed text rather than parsing as a number.
pub fn json_type(ty: marlowe_tools::ParamType) -> &'static str {
    use marlowe_tools::ParamType;
    match ty {
        ParamType::Integer | ParamType::Amount => "integer",
        ParamType::Boolean => "boolean",
        // Everything else is a string on the wire. What it MEANS — a path to scope, a URL to
        // allowlist, an id to resolve — is the permission layer's business, and encoding that in
        // the JSON type would tell the model about a mechanism it must not be able to address.
        ParamType::Text
        | ParamType::Path
        | ParamType::WritePath
        | ParamType::Url
        | ParamType::Identifier => "string",
    }
}

/// What a parameter is FOR, in words a model can act on.
///
/// **The tool's own words win.** Everything below is generated from `ty` and `required`, which told
/// a model that `run`'s `task` takes "text" — true, useless, and the reason a spawn delegated a
/// question its child had no way to answer. A generated sentence is the fallback for a parameter
/// whose name already says what it is, never a substitute for one that does not.
///
/// This branch existed only in the Ollama copy until ADR-060 collapsed the two; the hosted path had
/// been dropping every tool's own parameter prose since it was written.
pub fn param_description(p: &marlowe_tools::ParamSpec) -> String {
    use marlowe_tools::{ArgumentRole, ParamType};

    if let Some(d) = &p.description {
        let arity = if p.required { "REQUIRED" } else { "Optional" };
        return format!("{arity}. {d}");
    }
    let what = match p.ty {
        ParamType::Path => "an existing path, relative to the workspace root",
        ParamType::WritePath => "a path relative to the workspace root; it may not exist yet",
        ParamType::Url => "a full URL including the scheme, e.g. https://example.com/page",
        ParamType::Amount => "an amount in micros of the profile's currency",
        ParamType::Identifier => "an identifier returned by an earlier call",
        ParamType::Integer => "a whole number",
        ParamType::Boolean => "true or false",
        ParamType::Text => "text",
    };
    // Arity from `required`, and the role stated only where it changes what the model may do.
    // Rendering the role AS the arity is the same conflation the schema had, in prose, and it
    // reached the model a second time through `Engine::expected_params` after a refusal.
    let arity = if p.required { "REQUIRED" } else { "Optional" };
    match p.role {
        ArgumentRole::Target => format!("{arity}. What this acts on: {what}."),
        ArgumentRole::Payload => format!("{arity}. {what}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roles(m: &[Value]) -> Vec<String> {
        m.iter().map(|x| x["role"].as_str().unwrap_or("?").to_string()).collect()
    }

    #[test]
    fn an_empty_tool_array_is_omitted_and_a_populated_one_is_kept() {
        assert!(tools_field(json!([])).is_none());
        assert!(tools_field(json!([{ "type": "function" }])).is_some());
    }

    #[test]
    fn a_tool_message_with_no_preceding_call_becomes_a_user_message() {
        // The quarantined reader's request, exactly: a brief and a page, no call anywhere.
        let mut m = vec![
            json!({ "role": "system", "content": "Marlowe." }),
            json!({ "role": "assistant", "content": "Below are 1 fetched sources." }),
            json!({ "role": "tool", "content": "=== source_1 (web) ===\nbytes" }),
        ];
        unorphan_tool_messages(&mut m);
        assert_eq!(roles(&m), vec!["system", "assistant", "user"]);
        assert_eq!(m[2]["content"], "=== source_1 (web) ===\nbytes", "content is not rewritten");
    }

    #[test]
    fn a_tool_message_that_answers_a_real_call_keeps_its_role() {
        // The control. Without it the function above could return "demote everything" and pass.
        let mut m = vec![
            json!({ "role": "assistant", "tool_calls": [{ "id": "call_1" }] }),
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "ok" }),
        ];
        unorphan_tool_messages(&mut m);
        assert_eq!(roles(&m), vec!["assistant", "tool"]);
        assert_eq!(m[1]["tool_call_id"], "call_1");
    }

    #[test]
    fn a_result_whose_call_was_trimmed_away_is_demoted_and_loses_its_pairing() {
        // Compaction dropping an assistant turn while keeping its results reaches the same shape
        // the child does, by a route that has nothing to do with the quarantine.
        let mut m = vec![json!({
            "role": "tool", "tool_call_id": "call_9", "tool_name": "read", "content": "ok"
        })];
        unorphan_tool_messages(&mut m);
        assert_eq!(roles(&m), vec!["user"]);
        assert!(m[0].get("tool_call_id").is_none(), "a stale pairing is malformed on a user turn");
        assert!(m[0].get("tool_name").is_none());
    }

    #[test]
    fn a_reply_that_precedes_its_call_is_not_linked() {
        let mut m = vec![
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "ok" }),
            json!({ "role": "assistant", "tool_calls": [{ "id": "call_1" }] }),
        ];
        unorphan_tool_messages(&mut m);
        assert_eq!(roles(&m), vec!["user", "assistant"]);
    }
}
