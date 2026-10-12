//! The server side of MCP over stdio, for `marlowe mcp serve` (design X21): newline-delimited
//! JSON-RPC 2.0 with `initialize`, `ping`, `tools/list` and `tools/call`. What the tools are and
//! whether a call may run is the [`ServeTools`] implementation's business; this file only speaks
//! the protocol. Requests are answered in order on the calling thread.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::PROTOCOL_VERSION;

/// One tool as `tools/list` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServedTool {
    pub name: String,
    pub description: String,
    /// A JSON Schema object.
    pub input_schema: Value,
}

/// What `tools/call` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedResult {
    pub text: String,
    pub is_error: bool,
}

/// The tools a server offers and how a call runs.
pub trait ServeTools: Send + Sync {
    fn list(&self) -> Vec<ServedTool>;
    fn call(&self, name: &str, args: Value) -> ServedResult;
}

/// Who the server says it is in `initialize`.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// `instructions` in the `initialize` result.
    pub instructions: Option<String>,
}

/// The answer to one message, or `None` for a notification (or a response from the client).
pub fn handle(info: &ServerInfo, tools: &dyn ServeTools, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str);
    let (Some(id), Some(method)) = (id, method) else {
        return None;
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": result});
    let err = |code: i64, message: String| json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}});
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
            let mut result = json!({
                "protocolVersion": asked,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": info.name, "version": info.version},
            });
            if let Some(i) = &info.instructions {
                result["instructions"] = json!(i);
            }
            ok(result)
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = tools
                .list()
                .into_iter()
                .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
                .collect();
            ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(err(-32602, "tools/call needs `name`".into()));
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let r = tools.call(name, args);
            ok(json!({"content": [{"type": "text", "text": r.text}], "isError": r.is_error}))
        }
        "resources/list" => ok(json!({"resources": []})),
        "prompts/list" => ok(json!({"prompts": []})),
        other => err(-32601, format!("method not found: {other}")),
    })
}

/// Serves until `input` ends. A line that is not JSON gets a parse error with a null id.
pub fn serve(info: &ServerInfo, tools: &dyn ServeTools, input: impl BufRead, output: &mut dyn Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| handle(info, tools, m)).collect();
                (!replies.is_empty()).then(|| Value::Array(replies))
            }
            Ok(msg) => handle(info, tools, &msg),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
        };
        if let Some(r) = reply {
            writeln!(output, "{r}")?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Two;

    impl ServeTools for Two {
        fn list(&self) -> Vec<ServedTool> {
            vec![ServedTool { name: "echo".into(), description: "Echo.".into(), input_schema: json!({"type": "object"}) }]
        }
        fn call(&self, name: &str, args: Value) -> ServedResult {
            match name {
                "echo" => ServedResult { text: args["text"].as_str().unwrap_or("").into(), is_error: false },
                _ => ServedResult { text: format!("no tool {name}"), is_error: true },
            }
        }
    }

    fn info() -> ServerInfo {
        ServerInfo { name: "marlowe".into(), version: "0".into(), instructions: None }
    }

    #[test]
    fn a_session_initializes_lists_and_calls_in_order() {
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"sampling/createMessage"}"#,
            "not json",
        ]
        .join("\n");
        let mut out = Vec::new();
        serve(&info(), &Two, input.as_bytes(), &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 6, "the notification gets no answer");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "marlowe");
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "echo");
        assert_eq!(replies[2]["result"]["content"][0]["text"], "hi");
        assert_eq!(replies[3]["result"]["isError"], true);
        assert_eq!(replies[4]["error"]["code"], -32601);
        assert_eq!(replies[5]["error"]["code"], -32700);
    }
}
