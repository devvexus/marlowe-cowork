//! What `marlowe -p` writes: text, one json object, or stream-json lines (Claude Code's SDK shapes,
//! so an SDK client written for `claude -p --output-format stream-json` reads ours).
//!
//! stdout carries only the answer (or the JSON); every diagnostic goes to stderr.

use std::io::Write;

use marlowe_model::Usage;
use serde_json::{json, Value};

use super::args::OutputFormat;

/// The first stream-json line.
pub struct Init<'a> {
    pub session: &'a str,
    pub cwd: &'a str,
    pub model: &'a str,
    pub mode: &'a str,
    /// `(name, status)`.
    pub mcp_servers: Vec<(String, String)>,
    pub resumed: bool,
    /// `credential_manager`, `env` or `none` (see [`super::driver::key_source`]).
    pub api_key_source: &'a str,
}

/// One finished turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// `success`, `error_max_turns`, `error_max_budget_usd`, `error_during_execution`,
    /// `error_approval_needed`.
    pub subtype: &'static str,
    pub is_error: bool,
    pub text: String,
    pub session: String,
    pub duration_ms: u64,
    pub api_ms: u64,
    /// Model calls in the turn.
    pub num_turns: u32,
    pub usage: Usage,
    pub cost_usd: f64,
    pub denials: Vec<Value>,
    /// The daemon's stop reason (`done`, `interrupted`, `step_limit`, `budget`, `error`).
    pub stop: &'static str,
    pub exit_code: i32,
    pub error: Option<String>,
}

impl TurnResult {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "type": "result",
            "subtype": self.subtype,
            "is_error": self.is_error,
            "duration_ms": self.duration_ms,
            "duration_api_ms": self.api_ms,
            "num_turns": self.num_turns,
            "result": self.text,
            "session_id": self.session,
            "total_cost_usd": self.cost_usd,
            "usage": {
                "input_tokens": self.usage.input,
                "output_tokens": self.usage.output,
                "cache_read_input_tokens": self.usage.cached_input,
                "cache_creation_input_tokens": 0,
                "reasoning_tokens": self.usage.reasoning,
            },
            "permission_denials": self.denials,
            "stop": self.stop,
            "exit_code": self.exit_code,
        });
        if let Some(e) = &self.error {
            v["error"] = json!(e);
        }
        v
    }
}

pub struct Emitter {
    format: OutputFormat,
    partial: bool,
    verbose: bool,
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
}

impl Emitter {
    pub fn new(format: OutputFormat, partial: bool, verbose: bool, out: Box<dyn Write + Send>, err: Box<dyn Write + Send>) -> Self {
        Self { format, partial, verbose, out, err }
    }

    pub fn stream(&self) -> bool {
        self.format == OutputFormat::StreamJson
    }

    pub fn partial(&self) -> bool {
        self.stream() && self.partial
    }

    fn line(&mut self, v: &Value) {
        let _ = writeln!(self.out, "{v}");
        let _ = self.out.flush();
    }

    /// A diagnostic on stderr.
    pub fn warn(&mut self, msg: &str) {
        let _ = writeln!(self.err, "marlowe: {msg}");
        let _ = self.err.flush();
    }

    /// Progress on stderr, only under `--verbose`.
    pub fn progress(&mut self, msg: &str) {
        if self.verbose {
            let _ = writeln!(self.err, "{msg}");
            let _ = self.err.flush();
        }
    }

    pub fn system_init(&mut self, i: &Init<'_>) {
        if !self.stream() {
            return;
        }
        let servers: Vec<Value> = i.mcp_servers.iter().map(|(n, s)| json!({"name": n, "status": s})).collect();
        self.line(&json!({
            "type": "system",
            "subtype": "init",
            "cwd": i.cwd,
            "session_id": i.session,
            "model": i.model,
            "permissionMode": i.mode,
            "mcp_servers": servers,
            "resumed": i.resumed,
            "apiKeySource": i.api_key_source,
        }));
    }

    /// An assistant message: its text and, when the model called a tool, that call.
    pub fn assistant(&mut self, session: &str, model: &str, text: &str, tool_use: Option<(&str, &str, &Value)>) {
        if !self.stream() {
            return;
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        if let Some((id, name, input)) = tool_use {
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        if content.is_empty() {
            return;
        }
        self.line(&json!({
            "type": "assistant",
            "message": {"role": "assistant", "model": model, "content": content, "stop_reason": if tool_use.is_some() { "tool_use" } else { "end_turn" }},
            "parent_tool_use_id": null,
            "session_id": session,
        }));
    }

    pub fn tool_result(&mut self, session: &str, call_id: &str, ok: bool, content: &str) {
        if !self.stream() {
            return;
        }
        self.line(&json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content, "is_error": !ok}]},
            "parent_tool_use_id": null,
            "session_id": session,
        }));
    }

    /// `--include-partial-messages`: a text delta.
    pub fn text_delta(&mut self, session: &str, text: &str) {
        if !self.partial() {
            return;
        }
        self.line(&json!({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}},
            "session_id": session,
        }));
    }

    pub fn reasoning_delta(&mut self, session: &str, text: &str) {
        if !self.partial() || text.is_empty() {
            return;
        }
        self.line(&json!({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": text}},
            "session_id": session,
        }));
    }

    /// A request this process sends the SDK client (`can_use_tool`).
    pub fn control_request(&mut self, id: &str, request: Value) {
        self.line(&json!({"type": "control_request", "request_id": id, "request": request}));
    }

    /// The answer to a control request the client sent.
    pub fn control_response(&mut self, id: &str, result: Result<Value, String>) {
        let response = match result {
            Ok(v) => json!({"subtype": "success", "request_id": id, "response": v}),
            Err(e) => json!({"subtype": "error", "request_id": id, "error": e}),
        };
        self.line(&json!({"type": "control_response", "response": response}));
    }

    /// The end of a turn: the text answer, the one json object, or the `result` line.
    pub fn result(&mut self, r: &TurnResult) {
        match self.format {
            OutputFormat::Text => {
                if let Some(e) = &r.error {
                    self.warn(e);
                }
                if !r.text.is_empty() {
                    let _ = writeln!(self.out, "{}", r.text.trim_end());
                    let _ = self.out.flush();
                }
            }
            OutputFormat::Json | OutputFormat::StreamJson => self.line(&r.to_json()),
        }
    }

    /// A failure before any turn ran (nothing connected, a bad flag value found late).
    pub fn failure(&mut self, message: &str, session: &str, exit_code: i32) {
        match self.format {
            OutputFormat::Text => self.warn(message),
            _ => {
                self.warn(message);
                let r = TurnResult {
                    subtype: "error_during_execution",
                    is_error: true,
                    text: String::new(),
                    session: session.to_string(),
                    duration_ms: 0,
                    api_ms: 0,
                    num_turns: 0,
                    usage: Usage::default(),
                    cost_usd: 0.0,
                    denials: Vec::new(),
                    stop: "error",
                    exit_code,
                    error: Some(message.to_string()),
                };
                self.line(&r.to_json());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod capture {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A shared byte buffer standing in for stdout or stderr.
    #[derive(Clone, Default)]
    pub struct Buf(pub Arc<Mutex<Vec<u8>>>);

    impl Buf {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        /// The complete lines written so far, parsed (a line still being written is not one).
        pub fn lines(&self) -> Vec<serde_json::Value> {
            let text = self.text();
            let done = text.rfind('\n').map_or("", |i| &text[..=i]);
            done.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l}: {e}"))).collect()
        }
    }

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::capture::Buf;
    use super::*;

    fn emitter(format: OutputFormat, partial: bool) -> (Emitter, Buf, Buf) {
        let (o, e) = (Buf::default(), Buf::default());
        (Emitter::new(format, partial, true, Box::new(o.clone()), Box::new(e.clone())), o, e)
    }

    fn result(text: &str) -> TurnResult {
        TurnResult {
            subtype: "success",
            is_error: false,
            text: text.into(),
            session: "s1".into(),
            duration_ms: 5,
            api_ms: 3,
            num_turns: 2,
            usage: Usage { input: 10, output: 4, cached_input: 6, ..Default::default() },
            cost_usd: 0.25,
            denials: vec![],
            stop: "done",
            exit_code: 0,
            error: None,
        }
    }

    #[test]
    fn text_prints_only_the_answer_and_keeps_errors_on_stderr() {
        let (mut em, out, err) = emitter(OutputFormat::Text, false);
        em.system_init(&Init { session: "s", cwd: "c", model: "m", mode: "default", mcp_servers: vec![], resumed: false, api_key_source: "none" });
        em.assistant("s", "m", "thinking aloud", None);
        em.result(&result("the answer\n"));
        assert_eq!(out.text(), "the answer\n");
        assert_eq!(err.text(), "");
        let mut failed = result("");
        failed.error = Some("it broke".into());
        em.result(&failed);
        assert_eq!(err.text(), "marlowe: it broke\n");
    }

    #[test]
    fn json_is_one_result_object_with_usage_and_cost() {
        let (mut em, out, _) = emitter(OutputFormat::Json, false);
        em.assistant("s", "m", "x", None);
        em.result(&result("done"));
        let lines = out.lines();
        assert_eq!(lines.len(), 1);
        let v = &lines[0];
        assert_eq!(v["type"], "result");
        assert_eq!(v["subtype"], "success");
        assert_eq!(v["is_error"], false);
        assert_eq!(v["result"], "done");
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["num_turns"], 2);
        assert_eq!(v["total_cost_usd"], 0.25);
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 6);
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn stream_json_lines_follow_the_sdk_shapes() {
        let (mut em, out, _) = emitter(OutputFormat::StreamJson, true);
        em.system_init(&Init { session: "s1", cwd: "C:/w", model: "ollama/m", mode: "default", mcp_servers: vec![("fs".into(), "connected".into())], resumed: false, api_key_source: "env" });
        em.text_delta("s1", "he");
        em.assistant("s1", "ollama/m", "hello", Some(("c1", "bash", &json!({"summary": "ls"}))));
        em.tool_result("s1", "c1", true, "ok");
        em.control_request("r1", json!({"subtype": "can_use_tool", "tool_name": "bash"}));
        em.control_response("r2", Ok(json!({})));
        em.result(&result("fin"));
        let l = out.lines();
        assert_eq!(l.len(), 7);
        assert_eq!((l[0]["type"].as_str(), l[0]["subtype"].as_str()), (Some("system"), Some("init")));
        assert_eq!(l[0]["mcp_servers"][0]["name"], "fs");
        assert_eq!(l[1]["type"], "stream_event");
        assert_eq!(l[1]["event"]["delta"]["text"], "he");
        assert_eq!(l[2]["message"]["content"][0]["text"], "hello");
        assert_eq!(l[2]["message"]["content"][1]["type"], "tool_use");
        assert_eq!(l[3]["message"]["content"][0]["type"], "tool_result");
        assert_eq!(l[3]["message"]["content"][0]["tool_use_id"], "c1");
        assert_eq!(l[4]["request"]["subtype"], "can_use_tool");
        assert_eq!(l[5]["response"]["request_id"], "r2");
        assert_eq!(l[6]["type"], "result");
    }

    #[test]
    fn partial_lines_need_the_flag_and_a_failure_still_prints_a_result_line() {
        let (mut em, out, err) = emitter(OutputFormat::StreamJson, false);
        em.text_delta("s", "x");
        assert!(out.text().is_empty());
        em.failure("no daemon", "", 1);
        assert_eq!(out.lines()[0]["is_error"], true);
        assert_eq!(out.lines()[0]["exit_code"], 1);
        assert!(err.text().contains("no daemon"));
    }
}
