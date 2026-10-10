//! The `/api/chat` request body.
//!
//! Shape rules, each from a live failure:
//! - one system message at position 0 (several chat templates refuse any other placement);
//! - `tool_calls` on the assistant message with `arguments` as a JSON object, and `tool_name` +
//!   `tool_call_id` on each tool message, or the model cannot pair results with calls;
//! - only the newest assistant message keeps `thinking` (old reasoning compounds);
//! - `num_ctx` always sent (Ollama silently defaults to 2048);
//! - `tools` omitted rather than `[]`;
//! - leaked tool markup defanged so the template cannot choke on it.

use std::collections::HashMap;

use marlowe_model::{ChatRequest, Role};
use serde_json::{json, Map, Value};

use super::leak::defang_tool_markup;
use crate::wire;

/// Builds the body. `think` is the already-mapped `think` value (`None` omits it).
pub fn request_body(req: &ChatRequest, num_ctx: u32, think: Option<Value>) -> Value {
    let mut messages = Vec::new();

    // Leading system: the request's blocks plus any system messages before the first turn.
    let mut system = req.system_text();
    let lead = req.messages.iter().take_while(|m| m.role == Role::System).count();
    for m in &req.messages[..lead] {
        let t = m.joined_text();
        if !t.trim().is_empty() {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&t);
        }
    }
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }

    let mut names: HashMap<String, String> = HashMap::new();
    for m in &req.messages[lead..] {
        let text = m.joined_text();
        match m.role {
            Role::System => {
                if !text.trim().is_empty() {
                    messages.push(json!({ "role": "system", "content": text }));
                }
            }
            Role::User => {
                if !text.is_empty() || m.tool_results_iter().next().is_none() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let mut msg = Map::new();
                msg.insert("role".into(), "assistant".into());
                msg.insert("content".into(), Value::String(defang_tool_markup(&text)));
                if let Some(t) = m.thinking() {
                    if !t.text.is_empty() {
                        msg.insert("thinking".into(), Value::String(defang_tool_markup(&t.text)));
                    }
                }
                let calls: Vec<Value> = m
                    .tool_calls()
                    .map(|c| {
                        names.insert(c.id.clone(), c.name.clone());
                        let mut call = json!({
                            "function": { "name": c.name, "arguments": object_args(&c.args) }
                        });
                        if !c.id.is_empty() {
                            call["id"] = Value::String(c.id.clone());
                        }
                        call
                    })
                    .collect();
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(msg));
            }
            Role::Tool => {
                if m.tool_results_iter().next().is_none() && !text.is_empty() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
        }
        // Results travel as one `tool` message each, whatever message carried them.
        for r in m.tool_results_iter() {
            let mut msg = json!({
                "role": "tool",
                "content": r.content,
                "tool_call_id": r.call_id,
            });
            if let Some(name) = names.get(&r.call_id) {
                msg["tool_name"] = Value::String(name.clone());
            }
            messages.push(msg);
        }
    }

    wire::unorphan_tool_messages(&mut messages);

    let newest_assistant = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    for (i, m) in messages.iter_mut().enumerate() {
        if Some(i) != newest_assistant {
            if let Some(obj) = m.as_object_mut() {
                obj.remove("thinking");
            }
        }
    }

    let num_predict = req.max_output_tokens.unwrap_or(num_ctx / 4).min(num_ctx / 2).max(1);
    let mut options = json!({ "num_ctx": num_ctx, "num_predict": num_predict });
    if let Some(t) = req.temperature {
        options["temperature"] = json!(t);
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "options": options,
    });
    if let Some(think) = think {
        body["think"] = think;
    }
    if let Some(tools) = wire::tools_field(&req.tools) {
        body["tools"] = tools;
    }
    body
}

/// Ollama wants an object. A malformed call kept as a raw string is re-parsed or sent as `{}`.
fn object_args(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({}),
        },
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::{Message, Part, SystemBlock, Thinking, ToolCall, ToolResult, ToolSpec};

    fn convo() -> ChatRequest {
        let mut req = ChatRequest::new(
            "qwen3.5:9b",
            vec![
                Message::user("read a.md"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking { text: "old plan".into(), ..Default::default() }),
                        Part::Text { text: "Reading.".into() },
                        Part::ToolCall(ToolCall {
                            id: "call_1".into(),
                            name: "read".into(),
                            args: json!({"path": "a.md"}),
                        }),
                    ],
                },
                Message::tool_results([ToolResult {
                    call_id: "call_1".into(),
                    content: "hello".into(),
                    is_error: false,
                    parts: Vec::new(),
                }]),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking { text: "new plan".into(), ..Default::default() }),
                        Part::Text { text: "It says hello.".into() },
                    ],
                },
                Message::user("thanks"),
            ],
        );
        req.system = vec![
            SystemBlock { text: "core".into(), cache: true },
            SystemBlock { text: "<environment/>".into(), cache: false },
        ];
        req
    }

    #[test]
    fn one_system_message_leads_and_tool_results_pair_by_id_and_name() {
        let body = request_body(&convo(), 32_768, Some(json!(true)));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "core\n\n<environment/>");
        assert_eq!(msgs.iter().filter(|m| m["role"] == "system").count(), 1);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"]["path"], "a.md");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_name"], "read");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(body["options"]["num_ctx"], 32_768);
        assert_eq!(body["think"], true);
        assert!(body.get("tools").is_none(), "no tools means no key, never []");
    }

    #[test]
    fn only_the_newest_assistant_keeps_its_thinking() {
        let body = request_body(&convo(), 32_768, None);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[2].get("thinking").is_none());
        assert_eq!(msgs[4]["thinking"], "new plan");
        assert!(body.get("think").is_none());
    }

    #[test]
    fn output_cap_is_bounded_by_the_window_and_tools_are_sent() {
        let mut req = convo();
        req.max_output_tokens = Some(200_000);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let body = request_body(&req, 32_768, None);
        assert_eq!(body["options"]["num_predict"], 16_384);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn leaked_markup_in_history_is_defanged_on_the_way_out() {
        let req = ChatRequest::new(
            "m",
            vec![Message::user("hi"), Message::assistant("<function>read</function>")],
        );
        let body = request_body(&req, 8_192, None);
        let content = body["messages"][1]["content"].as_str().unwrap();
        assert!(!content.contains("<function"), "{content}");
    }

    #[test]
    fn a_malformed_argument_string_becomes_an_object() {
        assert_eq!(object_args(&json!("{\"a\":1}")), json!({"a": 1}));
        assert_eq!(object_args(&json!("{\"a\":")), json!({}));
    }
}
