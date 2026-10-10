//! The `/chat/completions` request body (OpenAI dialect).
//!
//! Differences from Ollama's `/api/chat`, each a live 400/500 somewhere:
//! - `arguments` is a JSON *string*, and every call carries `"type": "function"`;
//! - pairing is `tool_call_id` alone (no `tool_name`);
//! - reasoning goes back as the opaque `reasoning_details` the model produced, verbatim;
//! - one system message at position 0; for `anthropic/` models the cached system blocks carry
//!   `cache_control` breakpoints;
//! - `openrouter:`-prefixed tools are OpenRouter server tools, sent by type.

use marlowe_model::{ChatRequest, Role, SystemBlock, ToolSpec};
use marlowe_provider::wire;
use serde_json::{json, Map, Value};

/// Builds the body. `reasoning` is the already-mapped `reasoning` object (`None` omits it).
pub fn request_body(req: &ChatRequest, reasoning: Option<Value>) -> Value {
    let mut messages = Vec::new();

    let lead = req.messages.iter().take_while(|m| m.role == Role::System).count();
    let mut blocks: Vec<SystemBlock> = req.system.iter().filter(|b| !b.text.trim().is_empty()).cloned().collect();
    for m in &req.messages[..lead] {
        let t = m.joined_text();
        if !t.trim().is_empty() {
            blocks.push(SystemBlock { text: t, cache: false });
        }
    }
    if let Some(sys) = system_message(&blocks, req.model.starts_with("anthropic/")) {
        messages.push(sys);
    }

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
                let calls: Vec<Value> = m
                    .tool_calls()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": string_args(&c.args) },
                        })
                    })
                    .collect();
                let mut msg = Map::new();
                msg.insert("role".into(), "assistant".into());
                msg.insert(
                    "content".into(),
                    if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) },
                );
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                if let Some(details) = m.thinking().and_then(|t| t.details.clone()) {
                    msg.insert("reasoning_details".into(), details);
                }
                messages.push(Value::Object(msg));
            }
            Role::Tool => {
                if m.tool_results_iter().next().is_none() && !text.is_empty() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
        }
        for r in m.tool_results_iter() {
            messages.push(json!({ "role": "tool", "tool_call_id": r.call_id, "content": r.content }));
        }
    }

    wire::unorphan_tool_messages(&mut messages);

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "usage": { "include": true },
    });
    if let Some(n) = req.max_output_tokens {
        body["max_tokens"] = json!(n);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(tools) = tools_field(&req.tools) {
        body["tools"] = tools;
    }
    if let Some(r) = reasoning {
        body["reasoning"] = r;
    }
    body
}

/// Server tools (`openrouter:` prefix, e.g. `openrouter:web_search`) are sent as
/// `{"type": name}` plus `input_schema.parameters` when given; OpenRouter runs them itself.
/// Every other tool is a function tool.
fn tools_field(tools: &[ToolSpec]) -> Option<Value> {
    let (server, functions): (Vec<&ToolSpec>, Vec<&ToolSpec>) =
        tools.iter().partition(|t| t.name.starts_with(SERVER_TOOL_PREFIX));
    let functions: Vec<ToolSpec> = functions.into_iter().cloned().collect();
    let mut out = match wire::tools_field(&functions) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    for t in server {
        let mut tool = json!({ "type": t.name });
        if let Some(params) = t.input_schema.get("parameters").filter(|p| p.is_object()) {
            tool["parameters"] = params.clone();
        }
        out.push(tool);
    }
    (!out.is_empty()).then_some(Value::Array(out))
}

/// Tool names with this prefix are OpenRouter server tools.
pub const SERVER_TOOL_PREFIX: &str = "openrouter:";

fn system_message(blocks: &[SystemBlock], cache_control: bool) -> Option<Value> {
    if blocks.is_empty() {
        return None;
    }
    if cache_control && blocks.iter().any(|b| b.cache) {
        let parts: Vec<Value> = blocks
            .iter()
            .map(|b| {
                let mut p = json!({ "type": "text", "text": b.text });
                if b.cache {
                    p["cache_control"] = json!({ "type": "ephemeral" });
                }
                p
            })
            .collect();
        return Some(json!({ "role": "system", "content": parts }));
    }
    let text = blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
    Some(json!({ "role": "system", "content": text }))
}

/// `arguments` as a JSON string. A malformed call kept raw is sent as `{}` if it does not parse.
fn string_args(args: &Value) -> String {
    match args {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => v.to_string(),
            _ => "{}".to_string(),
        },
        Value::Object(_) => args.to_string(),
        _ => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::{Message, Part, Thinking, ToolCall, ToolResult};

    fn convo(model: &str) -> ChatRequest {
        let mut req = ChatRequest::new(
            model,
            vec![
                Message::user("read a.md"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking {
                            text: "plan".into(),
                            signature: None,
                            details: Some(json!([{"type": "reasoning.encrypted", "data": "xyz", "index": 0}])),
                        }),
                        Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a.md"}) }),
                    ],
                },
                Message::tool_results([ToolResult { call_id: "c1".into(), content: "hello".into(), is_error: false, parts: Vec::new() }]),
            ],
        );
        req.system = vec![
            SystemBlock { text: "core".into(), cache: true },
            SystemBlock { text: "env".into(), cache: false },
        ];
        req.tools = vec![ToolSpec { name: "read".into(), description: "Read".into(), input_schema: json!({"type":"object"}) }];
        req
    }

    #[test]
    fn tool_turns_use_string_arguments_and_round_trip_reasoning_details() {
        let body = request_body(&convo("openai/gpt-5"), Some(json!({"effort": "high"})));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "core\n\nenv"}));
        let call = &msgs[2]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"a.md\"}");
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["reasoning_details"][0]["data"], "xyz");
        assert_eq!(msgs[3], json!({"role": "tool", "tool_call_id": "c1", "content": "hello"}));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn server_tools_are_sent_by_type() {
        let mut req = ChatRequest::new("google/gemini-2.5-flash", vec![Message::user("news")]);
        req.tools = vec![ToolSpec {
            name: "openrouter:web_search".into(),
            description: "Server-side web search".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let body = request_body(&req, None);
        assert_eq!(body["tools"], json!([{"type": "openrouter:web_search"}]));
    }

    #[test]
    fn anthropic_models_get_cache_breakpoints_on_cached_blocks() {
        let body = request_body(&convo("anthropic/claude-sonnet-4.5"), None);
        let sys = &body["messages"][0]["content"];
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert!(sys[1].get("cache_control").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn an_orphaned_result_is_demoted_and_no_tools_means_no_key() {
        let mut req = ChatRequest::new(
            "x/y",
            vec![Message::tool_results([ToolResult { call_id: "ghost".into(), content: "page".into(), is_error: false, parts: Vec::new() }])],
        );
        req.max_output_tokens = Some(512);
        let body = request_body(&req, None);
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body.get("tools").is_none());
        assert_eq!(body["max_tokens"], 512);
    }
}
