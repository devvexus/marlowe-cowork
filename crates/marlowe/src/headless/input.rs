//! What a `--input-format stream-json` client writes on stdin: user messages and control messages
//! (Claude Code's SDK shapes), one JSON object per line.

use marlowe_proto::PermissionMode;
use serde_json::Value;

use super::args::parse_mode;

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// `{"type":"user","message":{"role":"user","content":"text" | [blocks]}}`.
    User { text: String },
    /// `control_request` with subtype `interrupt`.
    Interrupt { id: String },
    /// `control_request` with subtype `set_permission_mode`.
    SetMode { id: String, mode: PermissionMode },
    /// `control_request` with subtype `initialize` (answered with the run's capabilities).
    Initialize { id: String },
    /// `control_request` with subtype `answer_question`: the SDK answering a `QuestionAsked`.
    AnswerQuestion { id: String, question: String, answers: Vec<String> },
    /// A `control_response` to a request this process sent (`can_use_tool`).
    Response { request_id: String, result: Result<Value, String> },
    /// A well-formed control request this build does not know: answered with an error.
    UnknownControl { id: String, subtype: String },
}

fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str).unwrap_or("text") == "text")
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        Value::String(s) => vec![s.clone()],
        Value::Object(m) => m.values().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// Parses one stdin line. `Ok(None)` for a blank line.
pub fn parse_line(line: &str) -> Result<Option<Input>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(line).map_err(|e| format!("stdin: not JSON ({e})"))?;
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "user" => {
            let content = v.get("message").map(|m| m.get("content").unwrap_or(m)).or_else(|| v.get("content"));
            let text = content.and_then(text_of).ok_or("stdin: a user message needs text content")?;
            Ok(Some(Input::User { text }))
        }
        "control_request" => {
            let id = v.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
            let req = v.get("request").cloned().unwrap_or(Value::Null);
            let subtype = req.get("subtype").and_then(Value::as_str).unwrap_or("").to_string();
            Ok(Some(match subtype.as_str() {
                "interrupt" => Input::Interrupt { id },
                "initialize" => Input::Initialize { id },
                "set_permission_mode" => {
                    let mode = req.get("mode").and_then(Value::as_str).ok_or("set_permission_mode needs a mode")?;
                    Input::SetMode { mode: parse_mode(mode)?, id }
                }
                "answer_question" => Input::AnswerQuestion {
                    id,
                    question: req.get("id").or_else(|| req.get("question_id")).and_then(Value::as_str).unwrap_or("").to_string(),
                    answers: req.get("answers").map(string_list).unwrap_or_default(),
                },
                _ => Input::UnknownControl { id, subtype },
            }))
        }
        "control_response" => {
            let r = v.get("response").cloned().unwrap_or(Value::Null);
            let request_id = r.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
            let result = match r.get("subtype").and_then(Value::as_str) {
                Some("error") => Err(r.get("error").and_then(Value::as_str).unwrap_or("the client reported an error").to_string()),
                _ => Ok(r.get("response").cloned().unwrap_or(Value::Null)),
            };
            Ok(Some(Input::Response { request_id, result }))
        }
        // A client may echo `keep_alive` lines or send its own metadata; those carry nothing.
        "keep_alive" | "system" | "assistant" | "result" => Ok(None),
        other => Err(format!("stdin: unknown message type {other:?}")),
    }
}

/// The answers of a `can_use_tool` reply to `AskUserQuestion`: `updatedInput.answers` as a list,
/// or a map whose values are taken in order.
pub fn answers_of(updated: &Value) -> Vec<String> {
    updated.get("answers").map(string_list).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Input {
        parse_line(&v.to_string()).unwrap().unwrap()
    }

    #[test]
    fn user_messages_take_a_string_or_text_blocks() {
        assert_eq!(parse(json!({"type": "user", "message": {"role": "user", "content": "hello"}})), Input::User { text: "hello".into() });
        assert_eq!(
            parse(json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}})),
            Input::User { text: "a\nb".into() }
        );
        assert!(parse_line(r#"{"type":"user","message":{"role":"user","content":[]}}"#).is_err());
        assert_eq!(parse_line("   ").unwrap(), None);
        assert!(parse_line("not json").unwrap_err().contains("not JSON"));
        assert!(parse_line(r#"{"type":"wat"}"#).is_err());
    }

    #[test]
    fn control_requests() {
        assert_eq!(parse(json!({"type": "control_request", "request_id": "r1", "request": {"subtype": "interrupt"}})), Input::Interrupt { id: "r1".into() });
        assert_eq!(
            parse(json!({"type": "control_request", "request_id": "r2", "request": {"subtype": "set_permission_mode", "mode": "acceptEdits"}})),
            Input::SetMode { id: "r2".into(), mode: PermissionMode::AcceptEdits }
        );
        assert!(parse_line(r#"{"type":"control_request","request_id":"r","request":{"subtype":"set_permission_mode","mode":"yolo"}}"#).is_err());
        assert_eq!(
            parse(json!({"type": "control_request", "request_id": "r3", "request": {"subtype": "frobnicate"}})),
            Input::UnknownControl { id: "r3".into(), subtype: "frobnicate".into() }
        );
        assert_eq!(
            parse(json!({"type": "control_request", "request_id": "r4", "request": {"subtype": "answer_question", "id": "q1", "answers": ["a", "b"]}})),
            Input::AnswerQuestion { id: "r4".into(), question: "q1".into(), answers: vec!["a".into(), "b".into()] }
        );
    }

    #[test]
    fn control_responses_carry_success_or_error() {
        let ok = parse(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "x", "response": {"behavior": "allow"}}}));
        assert_eq!(ok, Input::Response { request_id: "x".into(), result: Ok(json!({"behavior": "allow"})) });
        let err = parse(json!({"type": "control_response", "response": {"subtype": "error", "request_id": "y", "error": "no"}}));
        assert_eq!(err, Input::Response { request_id: "y".into(), result: Err("no".into()) });
    }

    #[test]
    fn answers_come_from_a_list_or_a_map() {
        assert_eq!(answers_of(&json!({"answers": ["x", "y"]})), ["x", "y"]);
        assert_eq!(answers_of(&json!({"answers": {"Which?": "left"}})), ["left"]);
        assert!(answers_of(&json!({})).is_empty());
    }
}
