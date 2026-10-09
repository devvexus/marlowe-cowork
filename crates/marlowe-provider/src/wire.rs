//! Wire shapes shared by the OpenAI-style dialects (Ollama `/api/chat`, OpenRouter).
//!
//! Two shapes strict endpoints refuse with a 400: `tools: []` (omit the key instead), and a
//! `tool` message that answers no earlier assistant call (demoted to `user`).

use std::collections::BTreeSet;

use marlowe_model::ToolSpec;
use serde_json::{json, Value};

/// The role an unlinkable tool result is demoted to. Content is not rewritten.
const DEMOTED_ROLE: &str = "user";

/// Tools in the OpenAI function schema, or `None` when there are none (omit the key).
pub fn tools_field(tools: &[ToolSpec]) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    Some(Value::Array(
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": normalise_schema(&t.input_schema),
                    }
                })
            })
            .collect(),
    ))
}

/// A parameters schema must be an object schema; anything else becomes an empty one.
fn normalise_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(o) if !o.is_empty() => schema.clone(),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// Rewrites any `tool` message whose `tool_call_id` no earlier assistant message announced into
/// a plain user message, removing the pairing fields.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tools_omits_the_key() {
        assert!(tools_field(&[]).is_none());
        let spec = ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        };
        let t = tools_field(&[spec]).unwrap();
        assert_eq!(t[0]["type"], "function");
        assert_eq!(t[0]["function"]["name"], "read");
        assert_eq!(t[0]["function"]["parameters"]["properties"]["path"]["type"], "string");
    }

    #[test]
    fn a_null_schema_becomes_an_empty_object_schema() {
        let spec = ToolSpec { name: "now".into(), description: String::new(), input_schema: Value::Null };
        let t = tools_field(&[spec]).unwrap();
        assert_eq!(t[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn an_orphaned_tool_message_is_demoted_and_a_linked_one_is_kept() {
        let mut msgs = vec![
            json!({"role":"tool","tool_call_id":"x","content":"orphan"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"a"}]}),
            json!({"role":"tool","tool_call_id":"a","tool_name":"read","content":"ok"}),
        ];
        unorphan_tool_messages(&mut msgs);
        assert_eq!(msgs[0]["role"], "user");
        assert!(msgs[0].get("tool_call_id").is_none());
        assert_eq!(msgs[2]["role"], "tool");
    }
}
