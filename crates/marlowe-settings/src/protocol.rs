//! The hook protocol (design 2.6 X1-X9): Claude Code's input JSON per event and its output
//! schema, with Codex's generated JSON schemas as the conformance oracle
//! (`tests/fixtures/codex-hooks`, Apache-2.0).
//!
//! A command hook gets the event's input on stdin and answers with its exit code and output:
//!
//! | Exit | Meaning |
//! |---|---|
//! | 0 | success; stdout is JSON (below) or plain text: context for `UserPromptSubmit` / `SessionStart` / `SubagentStart`, a transcript note otherwise |
//! | 2 | blocking error: stderr is the reason (PreToolUse blocks the call, PermissionRequest denies, UserPromptSubmit drops the prompt, Stop/SubagentStop keep the agent going, PostToolUse(Failure) feeds it back); for the other events it is only shown |
//! | other | non-blocking error: stderr is shown, everything goes on |
//!
//! JSON fields: `continue` (false stops the turn, with `stopReason`), `suppressOutput`,
//! `systemMessage`, `decision` (`block`; PreToolUse also `approve`) with `reason`, and
//! `hookSpecificOutput` with `hookEventName` plus `permissionDecision` (`allow`/`deny`/`ask`),
//! `permissionDecisionReason`, `updatedInput` (PreToolUse), `additionalContext` (PreToolUse,
//! PostToolUse, UserPromptSubmit, SessionStart, SubagentStart), `updatedMCPToolOutput`
//! (PostToolUse), `decision{behavior, message, updatedInput, interrupt}` (PermissionRequest).

use marlowe_permission::rules::policy::HookPermission;
use serde_json::{json, Map, Value};

use crate::hooks::HookEvent;

/// What every event's input carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookContext {
    pub session_id: String,
    /// The session's transcript file, when it has one.
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub model: String,
    /// Claude Code's spelling: `default`, `acceptEdits`, `plan`, `dontAsk`, `bypassPermissions`.
    pub permission_mode: String,
    pub turn_id: String,
    /// Set inside a subagent.
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}

/// Claude Code's name for a Marlowe mode.
pub fn cc_mode(mode: marlowe_proto::PermissionMode) -> &'static str {
    use marlowe_proto::PermissionMode::*;
    match mode {
        Plan => "plan",
        Default => "default",
        AcceptEdits => "acceptEdits",
        Bypass => "bypassPermissions",
        DontAsk => "dontAsk",
    }
}

/// One event and its own fields.
#[derive(Debug, Clone, PartialEq)]
pub enum EventData {
    PreToolUse { tool_name: String, tool_input: Value, tool_use_id: String },
    PostToolUse { tool_name: String, tool_input: Value, tool_response: Value, tool_use_id: String },
    PostToolUseFailure { tool_name: String, tool_input: Value, tool_use_id: String, error: String },
    PermissionRequest { tool_name: String, tool_input: Value },
    UserPromptSubmit { prompt: String },
    /// `notification_type`: `permission_prompt`, `idle_prompt`, `elicitation_dialog`.
    Notification { message: String, notification_type: String },
    Stop { stop_hook_active: bool, last_assistant_message: String },
    SubagentStart { agent_id: String, agent_type: String },
    SubagentStop { agent_id: String, agent_type: String, agent_transcript_path: Option<String>, stop_hook_active: bool, last_assistant_message: String },
    /// `trigger`: `manual` or `auto`.
    PreCompact { trigger: String, custom_instructions: Option<String> },
    PostCompact { trigger: String },
    /// `source`: `startup`, `resume`, `clear`, `compact`.
    SessionStart { source: String },
    /// `reason`: `clear`, `logout`, `prompt_input_exit`, `other`.
    SessionEnd { reason: String },
}

impl EventData {
    pub fn event(&self) -> HookEvent {
        match self {
            EventData::PreToolUse { .. } => HookEvent::PreToolUse,
            EventData::PostToolUse { .. } => HookEvent::PostToolUse,
            EventData::PostToolUseFailure { .. } => HookEvent::PostToolUseFailure,
            EventData::PermissionRequest { .. } => HookEvent::PermissionRequest,
            EventData::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            EventData::Notification { .. } => HookEvent::Notification,
            EventData::Stop { .. } => HookEvent::Stop,
            EventData::SubagentStart { .. } => HookEvent::SubagentStart,
            EventData::SubagentStop { .. } => HookEvent::SubagentStop,
            EventData::PreCompact { .. } => HookEvent::PreCompact,
            EventData::PostCompact { .. } => HookEvent::PostCompact,
            EventData::SessionStart { .. } => HookEvent::SessionStart,
            EventData::SessionEnd { .. } => HookEvent::SessionEnd,
        }
    }

    /// What a matcher is tested against: the tool name, notification type, compaction trigger,
    /// start source, end reason or agent type. `None`: matchers do not apply (every hook runs).
    pub fn matcher_target(&self) -> Option<&str> {
        match self {
            EventData::PreToolUse { tool_name, .. }
            | EventData::PostToolUse { tool_name, .. }
            | EventData::PostToolUseFailure { tool_name, .. }
            | EventData::PermissionRequest { tool_name, .. } => Some(tool_name),
            EventData::Notification { notification_type, .. } => Some(notification_type),
            EventData::PreCompact { trigger, .. } | EventData::PostCompact { trigger } => Some(trigger),
            EventData::SessionStart { source } => Some(source),
            EventData::SessionEnd { reason } => Some(reason),
            EventData::SubagentStart { agent_type, .. } | EventData::SubagentStop { agent_type, .. } => Some(agent_type),
            EventData::UserPromptSubmit { .. } | EventData::Stop { .. } => None,
        }
    }

    /// The tool name a hook is told: Claude Code's name for Marlowe's built-ins (`Bash`,
    /// `Edit`), so a `.claude` hook script that checks `tool_name` works unchanged.
    fn told_tool(name: &str) -> String {
        crate::hooks::claude_tool_name(name).map_or_else(|| name.to_string(), str::to_string)
    }
}

/// Marlowe argument names and Claude Code's for the same tool (`path` is `file_path` for
/// `Edit`), so a `.claude` hook reading `.tool_input.file_path` works unchanged.
fn cc_arg_names(tool: &str) -> &'static [(&'static str, &'static str)] {
    match tool {
        "read" | "write" | "edit" | "multi_edit" => &[("path", "file_path")],
        "notebook_edit" => &[("path", "notebook_path")],
        _ => &[],
    }
}

/// `tool_input` as a hook sees it: Marlowe's arguments plus Claude Code's spelling of each.
pub fn cc_tool_input(tool: &str, input: &Value) -> Value {
    let mut out = input.clone();
    if let Value::Object(m) = &mut out {
        for (ours, theirs) in cc_arg_names(tool) {
            if let Some(v) = m.get(*ours).cloned() {
                m.entry(theirs.to_string()).or_insert(v);
            }
        }
    }
    out
}

/// A hook's `updatedInput` back in Marlowe's names: a Claude Code name the hook changed wins
/// over ours (`file_path` rewrites `path`), and the Claude Code spellings are dropped.
pub fn from_cc_input(tool: &str, original: &Value, updated: &Value) -> Value {
    let mut out = updated.clone();
    if let Value::Object(m) = &mut out {
        for (ours, theirs) in cc_arg_names(tool) {
            let Some(v) = m.remove(*theirs) else { continue };
            let changed = original.get(*ours) != Some(&v);
            if changed || !m.contains_key(*ours) {
                m.insert(ours.to_string(), v);
            }
        }
    }
    out
}

/// `tool_response` for PostToolUse: an object, as Claude Code sends (`stdout` for shells,
/// `filePath` for file tools), always with Marlowe's own text as `output`.
pub fn cc_tool_response(tool: &str, input: &Value, output: &str) -> Value {
    let mut m = Map::new();
    match tool {
        "bash" | "powershell" => {
            m.insert("stdout".into(), json!(output));
            m.insert("stderr".into(), json!(""));
            m.insert("interrupted".into(), json!(false));
            m.insert("isImage".into(), json!(false));
        }
        "read" | "write" | "edit" | "multi_edit" | "notebook_edit" => {
            if let Some(p) = input.get("path") {
                m.insert("filePath".into(), p.clone());
            }
            m.insert("success".into(), json!(true));
        }
        _ => {}
    }
    m.insert("output".into(), json!(output));
    Value::Object(m)
}

/// The JSON a hook reads on stdin. Fields follow Claude Code (and Codex's schemas): every event
/// carries `session_id`, `transcript_path`, `cwd`, `hook_event_name`; most carry `model`,
/// `permission_mode` and `turn_id`.
pub fn input_json(ctx: &HookContext, data: &EventData) -> Value {
    let event = data.event();
    let mut m = Map::new();
    m.insert("session_id".into(), json!(ctx.session_id));
    m.insert("transcript_path".into(), json!(ctx.transcript_path));
    m.insert("cwd".into(), json!(ctx.cwd));
    m.insert("hook_event_name".into(), json!(event.name()));
    let with_model = !matches!(data, EventData::SessionEnd { .. });
    let with_mode = !matches!(data, EventData::SessionEnd { .. } | EventData::PreCompact { .. } | EventData::PostCompact { .. });
    let with_turn = !matches!(data, EventData::SessionEnd { .. } | EventData::SessionStart { .. });
    let with_agent = !matches!(data, EventData::SessionEnd { .. } | EventData::SessionStart { .. } | EventData::Stop { .. } | EventData::Notification { .. });
    if with_model {
        m.insert("model".into(), json!(ctx.model));
    }
    if with_mode {
        m.insert("permission_mode".into(), json!(ctx.permission_mode));
    }
    if with_turn {
        m.insert("turn_id".into(), json!(ctx.turn_id));
    }
    if with_agent {
        if let Some(a) = &ctx.agent_id {
            m.insert("agent_id".into(), json!(a));
        }
        if let Some(a) = &ctx.agent_type {
            m.insert("agent_type".into(), json!(a));
        }
    }
    match data {
        EventData::PreToolUse { tool_name, tool_input, tool_use_id } => {
            m.insert("tool_name".into(), json!(EventData::told_tool(tool_name)));
            m.insert("tool_input".into(), cc_tool_input(tool_name, tool_input));
            m.insert("tool_use_id".into(), json!(tool_use_id));
        }
        EventData::PostToolUse { tool_name, tool_input, tool_response, tool_use_id } => {
            m.insert("tool_name".into(), json!(EventData::told_tool(tool_name)));
            m.insert("tool_input".into(), cc_tool_input(tool_name, tool_input));
            m.insert("tool_response".into(), tool_response.clone());
            m.insert("tool_use_id".into(), json!(tool_use_id));
        }
        EventData::PostToolUseFailure { tool_name, tool_input, tool_use_id, error } => {
            m.insert("tool_name".into(), json!(EventData::told_tool(tool_name)));
            m.insert("tool_input".into(), cc_tool_input(tool_name, tool_input));
            m.insert("tool_use_id".into(), json!(tool_use_id));
            m.insert("error".into(), json!(error));
        }
        EventData::PermissionRequest { tool_name, tool_input } => {
            m.insert("tool_name".into(), json!(EventData::told_tool(tool_name)));
            m.insert("tool_input".into(), cc_tool_input(tool_name, tool_input));
        }
        EventData::UserPromptSubmit { prompt } => {
            m.insert("prompt".into(), json!(prompt));
        }
        EventData::Notification { message, notification_type } => {
            m.insert("message".into(), json!(message));
            m.insert("notification_type".into(), json!(notification_type));
        }
        EventData::Stop { stop_hook_active, last_assistant_message } => {
            m.insert("stop_hook_active".into(), json!(stop_hook_active));
            m.insert("last_assistant_message".into(), json!(last_assistant_message));
        }
        EventData::SubagentStart { agent_id, agent_type } => {
            m.insert("agent_id".into(), json!(agent_id));
            m.insert("agent_type".into(), json!(agent_type));
        }
        EventData::SubagentStop { agent_id, agent_type, agent_transcript_path, stop_hook_active, last_assistant_message } => {
            m.insert("agent_id".into(), json!(agent_id));
            m.insert("agent_type".into(), json!(agent_type));
            m.insert("agent_transcript_path".into(), json!(agent_transcript_path));
            m.insert("stop_hook_active".into(), json!(stop_hook_active));
            m.insert("last_assistant_message".into(), json!(last_assistant_message));
        }
        EventData::PreCompact { trigger, custom_instructions } => {
            m.insert("trigger".into(), json!(trigger));
            if let Some(c) = custom_instructions.as_ref().filter(|c| !c.is_empty()) {
                m.insert("custom_instructions".into(), json!(c));
            }
        }
        EventData::PostCompact { trigger } => {
            m.insert("trigger".into(), json!(trigger));
        }
        EventData::SessionStart { source } => {
            m.insert("source".into(), json!(source));
        }
        EventData::SessionEnd { reason } => {
            m.insert("reason".into(), json!(reason));
        }
    }
    Value::Object(m)
}

/// How one hook process ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookRun {
    /// The command (or `prompt: ...`), for diagnostics.
    pub command: String,
    /// `None` when it was killed (timeout, cancel) or could not start.
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Why it could not start.
    pub spawn_error: Option<String>,
    /// The tool name, for diagnostics.
    pub tool: Option<String>,
}

/// A PermissionRequest hook's decision on the modal.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestDecision {
    pub allow: bool,
    pub message: Option<String>,
    pub updated_input: Option<Value>,
    /// Deny and stop the turn.
    pub interrupt: bool,
}

/// What the hooks of one event decided together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    /// A blocking answer (exit 2, `decision: "block"`, a PreToolUse deny): the reason. What it
    /// blocks depends on the event (see the module table).
    pub block: Option<String>,
    /// PreToolUse's `permissionDecision` (or the older `decision: approve|block`).
    pub permission: Option<HookPermission>,
    /// PreToolUse's `updatedInput`.
    pub updated_input: Option<Value>,
    /// Text for the model (`additionalContext`, plain stdout of context events).
    pub additional_context: Vec<String>,
    /// `continue: false`: stop the turn, with this reason for the user.
    pub stop: Option<String>,
    /// `systemMessage`s, shown to the user.
    pub system_messages: Vec<String>,
    /// `suppressOutput`: keep the hook's stdout out of the transcript.
    pub suppress_output: bool,
    /// Shown to the user: non-blocking errors' stderr, plain stdout of other events.
    pub notes: Vec<String>,
    /// Malformed output, timeouts, start failures.
    pub diagnostics: Vec<String>,
    pub permission_request: Option<PermissionRequestDecision>,
    /// PostToolUse's `updatedMCPToolOutput` (MCP tools only).
    pub updated_mcp_output: Option<Value>,
}

impl HookOutcome {
    /// Folds another hook's answer in: blocks and denials win, context accumulates, the last
    /// `updatedInput` stands.
    pub fn merge(&mut self, o: HookOutcome) {
        match (&mut self.block, o.block) {
            (Some(a), Some(b)) => {
                a.push('\n');
                a.push_str(&b);
            }
            (None, Some(b)) => self.block = Some(b),
            _ => {}
        }
        self.permission = match (self.permission.take(), o.permission) {
            (Some(HookPermission::Deny(a)), _) | (_, Some(HookPermission::Deny(a))) => Some(HookPermission::Deny(a)),
            (Some(HookPermission::Ask(a)), _) | (_, Some(HookPermission::Ask(a))) => Some(HookPermission::Ask(a)),
            (a, b) => b.or(a),
        };
        if o.updated_input.is_some() {
            self.updated_input = o.updated_input;
        }
        self.additional_context.extend(o.additional_context);
        if self.stop.is_none() {
            self.stop = o.stop;
        }
        self.system_messages.extend(o.system_messages);
        self.suppress_output |= o.suppress_output;
        self.notes.extend(o.notes);
        self.diagnostics.extend(o.diagnostics);
        self.permission_request = match (self.permission_request.take(), o.permission_request) {
            (Some(a), Some(b)) => Some(if !a.allow { a } else { b }),
            (a, b) => b.or(a),
        };
        if o.updated_mcp_output.is_some() {
            self.updated_mcp_output = o.updated_mcp_output;
        }
    }

    /// The text to append to the model's view (additional context and, for events whose block
    /// feeds back, the reason).
    pub fn context_text(&self) -> Option<String> {
        let parts: Vec<&str> = self.additional_context.iter().map(String::as_str).filter(|s| !s.trim().is_empty()).collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}

/// Events whose plain stdout is context for the model.
fn stdout_is_context(event: HookEvent) -> bool {
    matches!(event, HookEvent::UserPromptSubmit | HookEvent::SessionStart | HookEvent::SubagentStart)
}

fn preview(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one.chars().take(160).collect();
    if one.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// Interprets one hook's run for `event`.
pub fn interpret(event: HookEvent, run: &HookRun) -> HookOutcome {
    let mut o = HookOutcome::default();
    let stderr = run.stderr.trim();
    let stdout = run.stdout.trim();
    if let Some(e) = &run.spawn_error {
        o.diagnostics.push(format!("hook could not start: {e} (command={})", run.command));
        return o;
    }
    if run.timed_out || run.cancelled {
        o.diagnostics.push(format!(
            "hook {}: phase={} command={}",
            if run.timed_out { "timed out" } else { "cancelled" },
            event.name(),
            run.command
        ));
        return o;
    }
    match run.exit {
        Some(0) => {}
        Some(2) => {
            let reason = if stderr.is_empty() { format!("blocked by a {} hook", event.name()) } else { stderr.to_string() };
            match event {
                HookEvent::PreToolUse
                | HookEvent::PermissionRequest
                | HookEvent::UserPromptSubmit
                | HookEvent::Stop
                | HookEvent::SubagentStop
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure => o.block = Some(reason),
                _ => o.notes.push(reason),
            }
            if event == HookEvent::PreToolUse {
                o.permission = Some(HookPermission::Deny(o.block.clone().unwrap_or_default()));
            }
            return o;
        }
        _ => {
            let shown = if stderr.is_empty() { stdout } else { stderr };
            o.notes.push(format!("{} hook failed (exit {}): {}", event.name(), run.exit.map_or("?".into(), |c| c.to_string()), preview(shown)));
            return o;
        }
    }
    if stdout.is_empty() {
        return o;
    }
    if !stdout.starts_with('{') {
        if stdout_is_context(event) {
            o.additional_context.push(stdout.to_string());
        } else {
            o.notes.push(stdout.to_string());
        }
        return o;
    }
    let v: Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(e) => {
            o.diagnostics.push(format!(
                "hook_invalid_json: phase={} tool={} command={} detail={e} stdout_preview={} stderr_preview={}",
                event.name(),
                run.tool.as_deref().unwrap_or("-"),
                run.command,
                preview(stdout),
                preview(stderr),
            ));
            return o;
        }
    };
    apply_json(event, &v, &mut o);
    o
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Reads the JSON output fields into `o`.
pub fn apply_json(event: HookEvent, v: &Value, o: &mut HookOutcome) {
    if v.get("continue").and_then(Value::as_bool) == Some(false) {
        o.stop = Some(s(v, "stopReason").unwrap_or_else(|| format!("stopped by a {} hook", event.name())));
    }
    if let Some(m) = s(v, "systemMessage") {
        o.system_messages.push(m);
    }
    o.suppress_output |= v.get("suppressOutput").and_then(Value::as_bool).unwrap_or(false);
    let reason = s(v, "reason");
    match v.get("decision").and_then(Value::as_str) {
        Some("block") => {
            let why = reason.clone().unwrap_or_else(|| format!("blocked by a {} hook", event.name()));
            if event == HookEvent::PreToolUse {
                o.permission = Some(HookPermission::Deny(why.clone()));
            }
            o.block = Some(why);
        }
        Some("approve") if event == HookEvent::PreToolUse => o.permission = Some(HookPermission::Allow(reason.clone())),
        _ => {}
    }
    let Some(h) = v.get("hookSpecificOutput").filter(|h| h.is_object()) else { return };
    if let Some(c) = s(h, "additionalContext") {
        o.additional_context.push(c);
    }
    match event {
        HookEvent::PreToolUse => {
            let why = s(h, "permissionDecisionReason");
            match h.get("permissionDecision").and_then(Value::as_str) {
                Some("allow") => o.permission = Some(HookPermission::Allow(why)),
                Some("ask") => o.permission = Some(HookPermission::Ask(why)),
                Some("deny") => {
                    let why = why.unwrap_or_else(|| "denied by a PreToolUse hook".into());
                    o.block = Some(why.clone());
                    o.permission = Some(HookPermission::Deny(why));
                }
                _ => {}
            }
            if let Some(u) = h.get("updatedInput").filter(|u| u.is_object()) {
                o.updated_input = Some(u.clone());
            }
        }
        HookEvent::PostToolUse => {
            if let Some(u) = h.get("updatedMCPToolOutput").filter(|u| !u.is_null()) {
                o.updated_mcp_output = Some(u.clone());
            }
        }
        HookEvent::PermissionRequest => {
            if let Some(d) = h.get("decision").filter(|d| d.is_object()) {
                let allow = match d.get("behavior").and_then(Value::as_str) {
                    Some("allow") => true,
                    Some("deny") => false,
                    _ => return,
                };
                o.permission_request = Some(PermissionRequestDecision {
                    allow,
                    message: s(d, "message"),
                    updated_input: d.get("updatedInput").filter(|u| u.is_object()).cloned(),
                    interrupt: d.get("interrupt").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        _ => {}
    }
}
