//! Wire shapes shared by the OpenAI-style dialects (Ollama `/api/chat`, OpenRouter).
//!
//! Two shapes strict endpoints refuse with a 400: `tools: []` (omit the key instead), and a
//! `tool` message that answers no earlier assistant call (demoted to `user`).

use std::collections::BTreeSet;

use marlowe_model::ToolSpec;
use serde_json::{json, Value};

/// The role an unlinkable tool result is demoted to. Content is not rewritten.
const DEMOTED_ROLE: &str = "user";

/// Tools in the OpenAI function schema, or `None` when there are none (omit the key). Property
/// schemas go out as declared: the built-in specs leave plain strings typeless (Ollama reads them
/// fine and the tokens add up).
pub fn tools_field(tools: &[ToolSpec]) -> Option<Value> {
    build_tools(tools, false)
}

/// The same for a strict endpoint (OpenRouter): every untyped property is a `"type": "string"`.
pub fn tools_field_typed(tools: &[ToolSpec]) -> Option<Value> {
    build_tools(tools, true)
}

fn build_tools(tools: &[ToolSpec], typed: bool) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    Some(Value::Array(
        tools
            .iter()
            .map(|t| {
                let parameters = normalise_schema(&t.input_schema);
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": if typed { typed_schema(&parameters) } else { parameters },
                    }
                })
            })
            .collect(),
    ))
}

/// Gives every property of an object schema a `type`: a property with none (or only an `enum`)
/// is a string, an array with no `items` holds strings. Properties that already say what they
/// are (a type, `anyOf`, `$ref`, `const`, nested `properties`) are left alone.
pub fn typed_schema(schema: &Value) -> Value {
    fn prop(v: &Value) -> Value {
        let mut v = typed_schema(v);
        let Some(o) = v.as_object_mut() else { return v };
        let described = ["type", "anyOf", "oneOf", "allOf", "$ref", "const", "properties"];
        if !described.iter().any(|k| o.contains_key(*k)) {
            o.insert("type".into(), json!("string"));
        }
        if o.get("type") == Some(&json!("array")) && !o.contains_key("items") {
            o.insert("items".into(), json!({"type": "string"}));
        }
        v
    }
    let mut out = schema.clone();
    if let Some(o) = out.as_object_mut() {
        if let Some(Value::Object(props)) = o.get_mut("properties") {
            for v in props.values_mut() {
                *v = prop(v);
            }
        }
        if let Some(items) = o.get_mut("items") {
            *items = prop(items);
        }
    }
    out
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
    fn typed_schema_gives_untyped_properties_a_string_type() {
        let schema = json!({"type":"object","properties":{
            "a":{}, "b":{"enum":["x","y"]}, "c":{"type":"array"}, "d":{"type":"integer"},
            "e":{"type":"array","items":{"type":"object","properties":{"f":{}}}}}});
        let t = typed_schema(&schema);
        assert_eq!(t["properties"]["a"], json!({"type":"string"}));
        assert_eq!(t["properties"]["b"], json!({"enum":["x","y"],"type":"string"}));
        assert_eq!(t["properties"]["c"]["items"], json!({"type":"string"}));
        assert_eq!(t["properties"]["d"], json!({"type":"integer"}));
        assert_eq!(t["properties"]["e"]["items"]["properties"]["f"], json!({"type":"string"}));
        assert_eq!(tools_field(&[]), None);
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
